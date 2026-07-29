//! Binary file content for the diff view: read, decode, and cache.
//!
//! Bytes are read from the VCS once per diff load. Image decoding is the
//! expensive half, so it runs on a background thread and lands back through an
//! mpsc channel that the event loop drains, exactly like the PR fetches.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::{Resize, protocol::Protocol};

use super::*;
use crate::model::comment::LineSide;
use crate::ui::binary_view::{
    BinaryBlock, BinaryFacts, FileKind, HEX_PREVIEW_BYTES, SideFacts, detect_kind,
    has_image_extension,
};

/// Columns the cursor indicator takes before a binary block's content.
pub const BINARY_BLOCK_INDENT: u16 = 2;

/// Largest file whose bytes we keep. Past this we still report size, type, and
/// the hex preview, but drop the body rather than hold a huge buffer per side
/// for the whole session.
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// Identifies one side of one binary file.
pub type BinaryImageKey = (PathBuf, LineSide);

/// Bytes and derived facts for one side of a binary file.
#[derive(Debug, Default, Clone)]
pub struct BinarySide {
    /// Retained bytes, absent when the side does not exist or is oversized.
    pub bytes: Option<Arc<Vec<u8>>>,
    /// Leading bytes kept for the hex dump even when `bytes` was dropped.
    pub preview: Vec<u8>,
    pub size: Option<u64>,
    pub kind: Option<FileKind>,
    pub dimensions: Option<(u32, u32)>,
}

impl BinarySide {
    fn facts(&self) -> SideFacts {
        SideFacts {
            size: self.size,
            kind: self.kind,
            dimensions: self.dimensions,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct BinaryFileContent {
    pub old: BinarySide,
    pub new: BinarySide,
}

impl BinaryFileContent {
    fn side(&self, side: LineSide) -> &BinarySide {
        match side {
            LineSide::Old => &self.old,
            LineSide::New => &self.new,
        }
    }
}

/// A decoded image, or the reason there isn't one.
pub enum DecodedImage {
    Decoding,
    /// Decoding failed; the metadata card stands in.
    Failed,
    Ready(Box<DynamicImage>),
}

impl App {
    /// Read every binary file's content for the current diff.
    ///
    /// Mirrors `populate_file_line_count_cache`: called once per diff load,
    /// keyed by display path so re-sorting the file list does not invalidate
    /// it. Decoding is handed to a background thread before returning.
    pub(in crate::app) fn populate_binary_cache(&mut self) {
        self.binary_content.clear();
        self.binary_images.clear();
        self.binary_protocols.clear();
        self.binary_decode_rx = None;

        let (old_rev, new_rev) = self.binary_revs();
        let binaries: Vec<(PathBuf, Option<PathBuf>, Option<PathBuf>)> = self
            .diff_files
            .iter()
            .filter(|file| file.is_binary)
            .map(|file| {
                (
                    file.display_path().clone(),
                    file.old_path.clone(),
                    file.new_path.clone(),
                )
            })
            .collect();
        if binaries.is_empty() {
            return;
        }

        let mut decode_jobs: Vec<(BinaryImageKey, Arc<Vec<u8>>)> = Vec::new();
        for (display_path, old_path, new_path) in binaries {
            let mut content = BinaryFileContent::default();
            for (side, path, rev) in [
                (LineSide::Old, old_path, old_rev.as_deref()),
                (LineSide::New, new_path, new_rev.as_deref()),
            ] {
                let Some(path) = path else { continue };
                let bytes = self
                    .vcs
                    .read_file_bytes(&path, rev)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if bytes.is_empty() {
                    continue;
                }
                let filled = build_side(bytes, &path);
                if let Some(bytes) = filled.bytes.clone()
                    && filled.kind.is_some_and(|k| k.is_image())
                {
                    decode_jobs.push(((display_path.clone(), side), bytes));
                    self.binary_images
                        .insert((display_path.clone(), side), DecodedImage::Decoding);
                }
                match side {
                    LineSide::Old => content.old = filled,
                    LineSide::New => content.new = filled,
                }
            }
            self.binary_content.insert(display_path, content);
        }

        if !decode_jobs.is_empty() {
            self.binary_decode_rx = Some(spawn_decode(decode_jobs));
        }
    }

    /// Populate the binary cache when it does not match the current diff.
    ///
    /// `diff_files` is assigned from a dozen places (local loads, commit
    /// ranges, PR opens and reloads); rather than hook each one, this runs from
    /// `rebuild_annotations`, which every one of them already has to call.
    pub(in crate::app) fn ensure_binary_cache(&mut self) {
        let covered = self
            .diff_files
            .iter()
            .filter(|file| file.is_binary)
            .all(|file| self.binary_content.contains_key(file.display_path()));
        let stale =
            self.binary_content.len() != self.diff_files.iter().filter(|f| f.is_binary).count();
        if !covered || stale {
            self.populate_binary_cache();
        }
    }

    /// Drain finished image decodes. Returns true when a redraw is due.
    pub fn poll_binary_decode_events(&mut self) -> bool {
        let Some(rx) = &self.binary_decode_rx else {
            return false;
        };
        let mut received = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok((key, decoded)) => {
                    self.binary_images.insert(
                        key,
                        match decoded {
                            Some(image) => DecodedImage::Ready(Box::new(image)),
                            None => DecodedImage::Failed,
                        },
                    );
                    received = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.binary_decode_rx = None;
        }
        received
    }

    /// The rendered block for one binary file at the current viewport geometry.
    ///
    /// Both diff renderers and `rebuild_annotations` go through here, so the
    /// row count they each assume can never drift apart.
    pub fn binary_block(&self, display_path: &Path) -> BinaryBlock<'static> {
        let width = (self.diff_state.viewport_width as u16).saturating_sub(BINARY_BLOCK_INDENT);
        let mut facts = self.binary_facts(display_path);
        // No picker means nothing can paint an image, so do not reserve rows
        // for one; the metadata card carries the file instead.
        if self.image_picker.is_none() {
            facts.old.kind = facts.old.kind.map(FileKind::demote_image);
            facts.new.kind = facts.new.kind.map(FileKind::demote_image);
        }
        crate::ui::binary_view::binary_block(
            &self.theme,
            &facts,
            width,
            self.diff_state.viewport_height as u16,
        )
    }

    /// The view model the diff renderers and `rebuild_annotations` share.
    pub fn binary_facts(&self, display_path: &Path) -> BinaryFacts<'_> {
        let Some(content) = self.binary_content.get(display_path) else {
            return BinaryFacts {
                old: SideFacts::default(),
                new: SideFacts::default(),
                preview: &[],
                preview_total: 0,
                loading: false,
            };
        };
        // The new side is what a reviewer cares about; fall back to the old
        // side so a deleted file still shows what it held.
        let source = if content.new.preview.is_empty() {
            &content.old
        } else {
            &content.new
        };
        BinaryFacts {
            old: content.old.facts(),
            new: content.new.facts(),
            preview: &source.preview,
            preview_total: source.size.unwrap_or_default(),
            loading: false,
        }
    }

    /// The encoded image protocol for one pane, sized to `size`.
    ///
    /// Encoding is cached and only redone when the pane's cell size changes,
    /// so a steady viewport encodes each image exactly once.
    pub fn binary_image_protocol(&mut self, key: &BinaryImageKey, size: Size) -> Option<&Protocol> {
        if size.width == 0 || size.height == 0 {
            return None;
        }
        if !matches!(self.binary_protocols.get(key), Some((cached, _)) if *cached == size) {
            let picker = self.image_picker.as_ref()?;
            let DecodedImage::Ready(image) = self.binary_images.get(key)? else {
                return None;
            };
            let protocol = picker
                .new_protocol(image.as_ref().clone(), size, Resize::Scale(None))
                .ok()?;
            self.binary_protocols.insert(key.clone(), (size, protocol));
        }
        self.binary_protocols.get(key).map(|(_, protocol)| protocol)
    }

    /// Whether one side of a binary file has bytes to show.
    pub fn binary_side_present(&self, display_path: &Path, side: LineSide) -> bool {
        self.binary_content
            .get(display_path)
            .is_some_and(|content| content.side(side).size.is_some())
    }

    /// Revision specs for the old and new sides of the current diff.
    ///
    /// `None` means the working tree. A commit range reads the parent of its
    /// oldest commit against its newest, matching what the diff itself spans.
    fn binary_revs(&self) -> (Option<String>, Option<String>) {
        match &self.diff_source {
            DiffSource::CommitRange(commits) => {
                let oldest = match self.commit_selection_range {
                    Some((_, end)) => self.review_commits.get(end).map(|c| c.id.clone()),
                    None => commits.first().cloned(),
                };
                (
                    oldest.map(|id| format!("{id}^")),
                    self.ref_commit().map(str::to_string),
                )
            }
            DiffSource::PullRequest(pr) => {
                (Some(pr.base_sha.clone()), Some(pr.key.head_sha.clone()))
            }
            _ => (Some("HEAD".to_string()), None),
        }
    }
}

/// Derives one side's facts from its bytes.
///
/// Magic bytes decide the type. The extension only speaks when they say
/// nothing: an `.png` we cannot identify is labelled as unreadable image data
/// rather than left as a nameless blob, and still falls to the metadata card.
fn build_side(bytes: Vec<u8>, path: &Path) -> BinarySide {
    let size = bytes.len() as u64;
    let kind = match detect_kind(&bytes) {
        FileKind::Unknown if has_image_extension(path) => {
            FileKind::Other("unrecognized image data")
        }
        kind => kind,
    };
    let dimensions = kind
        .is_image()
        .then(|| image::ImageReader::new(std::io::Cursor::new(&bytes)))
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| reader.into_dimensions().ok());
    let preview = bytes.iter().take(HEX_PREVIEW_BYTES).copied().collect();

    BinarySide {
        bytes: (bytes.len() <= MAX_RETAINED_BYTES).then(|| Arc::new(bytes)),
        preview,
        size: Some(size),
        kind: Some(kind),
        dimensions,
    }
}

/// Longest edge kept after decoding.
///
/// The render loop rescales the decoded image every time a pane's size
/// changes, which happens on every scroll step. Capping the source here keeps
/// that rescale cheap no matter how large the file is; no terminal has the
/// cells to show more detail anyway.
const MAX_DECODED_EDGE: u32 = 1024;

/// Decode every queued image off the main thread.
///
/// A single thread walks the queue so a diff full of images cannot spawn an
/// unbounded number of them; results stream back one at a time.
fn spawn_decode(
    jobs: Vec<(BinaryImageKey, Arc<Vec<u8>>)>,
) -> Receiver<(BinaryImageKey, Option<DynamicImage>)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (key, bytes) in jobs {
            let decoded = image::load_from_memory(&bytes).ok().map(|image| {
                if image.width().max(image.height()) > MAX_DECODED_EDGE {
                    image.thumbnail(MAX_DECODED_EDGE, MAX_DECODED_EDGE)
                } else {
                    image
                }
            });
            if tx.send((key, decoded)).is_err() {
                return;
            }
        }
    });
    rx
}
