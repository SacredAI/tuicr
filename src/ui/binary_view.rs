//! Binary file presentation for the diff view.
//!
//! Images get an old/new pair of panes that `ratatui-image` paints over
//! reserved blank rows; every other binary gets a metadata card with an
//! `xxd`-style hex dump of the surviving side.

use std::path::Path;

use ratatui::text::{Line, Span};

use crate::model::comment::LineSide;
use crate::theme::Theme;
use crate::ui::styles;
use crate::vcs::lfs::LfsMissing;

/// Bytes shown in the hex dump preview.
pub const HEX_PREVIEW_BYTES: usize = 256;
const HEX_BYTES_PER_ROW: usize = 16;

/// Rows reserved for one image pane, before clamping to the viewport.
const IMAGE_ROWS: u16 = 18;
/// Rows the image block needs besides the image itself: a label per pane and a
/// trailing blank. The viewport clamp subtracts this so a block that scrolls
/// fully into view always shows its image.
const IMAGE_CHROME_ROWS: u16 = 3;
/// Narrowest pane worth splitting into; below twice this the panes stack.
const MIN_PANE_WIDTH: u16 = 24;

/// A binary file's type as identified from its leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A raster image `image` can decode, named for display.
    Image(&'static str),
    /// A recognised non-image format.
    Other(&'static str),
    Unknown,
}

impl FileKind {
    pub fn label(&self) -> &'static str {
        match self {
            FileKind::Image(name) | FileKind::Other(name) => name,
            FileKind::Unknown => "unknown format",
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, FileKind::Image(_))
    }

    /// Keeps the type name but drops the claim that it can be shown as one.
    pub fn demote_image(self) -> Self {
        match self {
            FileKind::Image(name) => FileKind::Other(name),
            kind => kind,
        }
    }
}

/// Identifies a file type from its magic bytes.
///
/// Magic bytes are authoritative: a `.png` holding JPEG data reports JPEG, and
/// a file whose bytes match no known signature is `Unknown` whatever it is
/// named. [`has_image_extension`] only supplies a guess when no bytes exist.
pub fn detect_kind(bytes: &[u8]) -> FileKind {
    let starts = |sig: &[u8]| bytes.starts_with(sig);

    if starts(b"\x89PNG\r\n\x1a\n") {
        FileKind::Image("PNG image")
    } else if starts(b"\xff\xd8\xff") {
        FileKind::Image("JPEG image")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        FileKind::Image("GIF image")
    } else if starts(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        FileKind::Image("WebP image")
    } else if starts(b"BM") {
        FileKind::Image("BMP image")
    } else if starts(b"%PDF-") {
        FileKind::Other("PDF document")
    } else if starts(b"PK\x03\x04") {
        FileKind::Other("ZIP archive")
    } else if starts(b"\x1f\x8b") {
        FileKind::Other("gzip archive")
    } else if starts(b"\x7fELF") {
        FileKind::Other("ELF binary")
    } else if starts(b"\xcf\xfa\xed\xfe") || starts(b"\xca\xfe\xba\xbe") {
        FileKind::Other("Mach-O binary")
    } else if starts(b"\0asm") {
        FileKind::Other("WebAssembly module")
    } else if starts(b"OggS") {
        FileKind::Other("Ogg container")
    } else if starts(b"ID3") {
        FileKind::Other("MP3 audio")
    } else if starts(b"\0\x01\0\0") || starts(b"OTTO") || starts(b"true") {
        FileKind::Other("font")
    } else if starts(b"SQLite format 3\0") {
        FileKind::Other("SQLite database")
    } else {
        FileKind::Unknown
    }
}

/// Whether the path names one of the raster formats we can decode.
pub fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"))
}

/// Formats a byte count with a binary-prefix unit.
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match bytes {
        b if b < KIB => format!("{b} B"),
        b if b < MIB => format!("{:.1} KiB", b as f64 / KIB as f64),
        b if b < GIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        b => format!("{:.1} GiB", b as f64 / GIB as f64),
    }
}

/// Formats the old-to-new size change, or `None` when a side is absent.
pub fn format_size_delta(old: Option<u64>, new: Option<u64>) -> Option<String> {
    let (old, new) = (old?, new?);
    if old == new {
        return Some("no size change".to_string());
    }
    let (sign, magnitude) = if new > old {
        ('+', new - old)
    } else {
        ('-', old - new)
    };
    Some(format!("{sign}{}", format_size(magnitude)))
}

/// Renders `bytes` as `xxd`-style rows: offset, hex columns, ASCII gutter.
///
/// Bytes past `HEX_PREVIEW_BYTES` are dropped; the caller reports the
/// truncation. A short final row is padded so the ASCII gutter stays aligned.
pub fn hex_dump(bytes: &[u8]) -> Vec<String> {
    let capped = &bytes[..bytes.len().min(HEX_PREVIEW_BYTES)];
    capped
        .chunks(HEX_BYTES_PER_ROW)
        .enumerate()
        .map(|(row, chunk)| {
            let mut hex = String::with_capacity(HEX_BYTES_PER_ROW * 3 + 1);
            let mut ascii = String::with_capacity(HEX_BYTES_PER_ROW);
            for (i, byte) in chunk.iter().enumerate() {
                if i == HEX_BYTES_PER_ROW / 2 {
                    hex.push(' ');
                }
                hex.push_str(&format!("{byte:02x} "));
                ascii.push(if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '.'
                });
            }
            let hex_width = HEX_BYTES_PER_ROW * 3 + 1;
            format!(
                "{:08x}  {hex:<hex_width$} |{ascii}|",
                row * HEX_BYTES_PER_ROW
            )
        })
        .collect()
}

/// A git-LFS object the diff could not show, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsNotice {
    pub oid: String,
    pub reason: LfsMissing,
}

impl LfsNotice {
    fn summary(&self) -> &'static str {
        match self.reason {
            LfsMissing::NotFetched => "LFS object not fetched",
            LfsMissing::TooLarge => "LFS object too large to show",
        }
    }

    /// The card's line: what is wrong, which object, and what to do about it.
    fn detail(&self, size: Option<u64>) -> String {
        let size = size.map(format_size).unwrap_or_else(|| "?".to_string());
        let oid = &self.oid[..self.oid.len().min(12)];
        match self.reason {
            LfsMissing::NotFetched => {
                format!("LFS object not present locally (oid {oid}…, {size}) — run `git lfs fetch`")
            }
            LfsMissing::TooLarge => {
                format!("LFS object too large to show (oid {oid}…, {size})")
            }
        }
    }
}

/// What one side of a binary diff holds, as far as the VCS could tell us.
#[derive(Debug, Clone, Default)]
pub struct SideFacts {
    /// `None` when the side does not exist (added/deleted) or was unreadable.
    pub size: Option<u64>,
    pub kind: Option<FileKind>,
    pub dimensions: Option<(u32, u32)>,
    /// Set when the side is an LFS pointer whose content we could not resolve;
    /// `size` then comes from the pointer, not from bytes we hold.
    pub lfs: Option<LfsNotice>,
}

impl SideFacts {
    fn label(&self, side: LineSide) -> String {
        let name = match side {
            LineSide::Old => "old",
            LineSide::New => "new",
        };
        let Some(size) = self.size else {
            return format!("{name}: (none)");
        };
        if let Some(notice) = &self.lfs {
            return format!("{name}: {}, {}", format_size(size), notice.summary());
        }
        match self.dimensions {
            Some((w, h)) => format!("{name}: {w}×{h}, {}", format_size(size)),
            None => format!("{name}: {}", format_size(size)),
        }
    }

    /// Whether an image pane can be painted for this side.
    fn has_content(&self) -> bool {
        self.size.is_some() && self.lfs.is_none()
    }
}

/// Both sides of one binary file, plus the bytes the hex dump previews.
pub struct BinaryFacts<'a> {
    pub old: SideFacts,
    pub new: SideFacts,
    /// New-side bytes, or old-side for a deleted file. Empty while loading.
    pub preview: &'a [u8],
    /// Full size of the side `preview` came from, which `preview` truncates.
    pub preview_total: u64,
    pub loading: bool,
}

impl BinaryFacts<'_> {
    fn is_image(&self) -> bool {
        [&self.old, &self.new]
            .iter()
            .any(|s| s.kind.is_some_and(|k| k.is_image()))
    }

    fn kind(&self) -> FileKind {
        self.new.kind.or(self.old.kind).unwrap_or(FileKind::Unknown)
    }
}

/// Where one image pane sits inside the reserved block, in block-local rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePane {
    pub side: LineSide,
    /// Rows from the top of the block to the pane's first image row.
    pub row: u16,
    /// Columns from the left of the block content to the pane's left edge.
    pub column: u16,
    pub width: u16,
    pub height: u16,
}

/// The rendered form of one binary file: text rows plus any image panes.
///
/// `lines` carries no cursor indicator; callers prefix their own. `lines.len()`
/// is the block's row count and is the single source of truth shared by the
/// renderers and `rebuild_annotations`.
pub struct BinaryBlock<'a> {
    pub lines: Vec<Line<'a>>,
    pub panes: Vec<ImagePane>,
}

/// Builds the block for one binary file at the given content width and
/// viewport height.
///
/// Row count depends only on the facts and the geometry, never on whether an
/// image has finished decoding, so the block does not resize under the cursor
/// once decoding completes.
pub fn binary_block<'a>(
    theme: &Theme,
    facts: &BinaryFacts<'_>,
    width: u16,
    viewport_height: u16,
) -> BinaryBlock<'a> {
    if facts.loading {
        return BinaryBlock {
            lines: vec![Line::from(Span::styled(
                "(binary file — reading…)",
                styles::dim_style(theme),
            ))],
            panes: Vec::new(),
        };
    }

    if facts.is_image() {
        image_block(theme, facts, width, viewport_height)
    } else {
        metadata_block(theme, facts)
    }
}

fn image_block<'a>(
    theme: &Theme,
    facts: &BinaryFacts<'_>,
    width: u16,
    viewport_height: u16,
) -> BinaryBlock<'a> {
    let stacked = width < MIN_PANE_WIDTH * 2 + 1;
    let image_rows = IMAGE_ROWS.min(
        viewport_height
            .saturating_sub(IMAGE_CHROME_ROWS * if stacked { 2 } else { 1 })
            .max(1),
    );

    let mut lines = Vec::new();
    let mut panes = Vec::new();
    let dim = styles::dim_style(theme);

    if stacked {
        for side in [LineSide::Old, LineSide::New] {
            let facts_for = side_facts(facts, side);
            lines.push(Line::from(Span::styled(facts_for.label(side), dim)));
            if facts_for.has_content() {
                panes.push(ImagePane {
                    side,
                    row: lines.len() as u16,
                    column: 0,
                    width,
                    height: image_rows,
                });
            }
            lines.extend((0..image_rows).map(|_| Line::default()));
        }
    } else {
        let pane_width = (width.saturating_sub(1)) / 2;
        let label = format!(
            "{:<pane_width$} {}",
            facts.old.label(LineSide::Old),
            facts.new.label(LineSide::New),
            pane_width = pane_width as usize,
        );
        lines.push(Line::from(Span::styled(label, dim)));
        for (side, column) in [(LineSide::Old, 0), (LineSide::New, pane_width + 1)] {
            if side_facts(facts, side).has_content() {
                panes.push(ImagePane {
                    side,
                    row: 1,
                    column,
                    width: pane_width,
                    height: image_rows,
                });
            }
        }
        lines.extend((0..image_rows).map(|_| Line::default()));
    }

    lines.push(Line::default());
    BinaryBlock { lines, panes }
}

fn side_facts<'a>(facts: &'a BinaryFacts<'_>, side: LineSide) -> &'a SideFacts {
    match side {
        LineSide::Old => &facts.old,
        LineSide::New => &facts.new,
    }
}

fn metadata_block<'a>(theme: &Theme, facts: &BinaryFacts<'_>) -> BinaryBlock<'a> {
    let dim = styles::dim_style(theme);
    let header = styles::diff_hunk_header_style(theme);
    let mut lines = vec![Line::from(Span::styled(
        format!("Binary file — {}", facts.kind().label()),
        header,
    ))];

    lines.push(Line::from(Span::styled(
        facts.old.label(LineSide::Old),
        dim,
    )));
    let mut new_label = facts.new.label(LineSide::New);
    if let Some(delta) = format_size_delta(facts.old.size, facts.new.size) {
        new_label.push_str(&format!("  ({delta})"));
    }
    lines.push(Line::from(Span::styled(new_label, dim)));
    lines.push(Line::default());

    for side in [&facts.old, &facts.new] {
        if let Some(notice) = &side.lfs {
            lines.push(Line::from(Span::styled(notice.detail(side.size), dim)));
        }
    }

    if facts.preview.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no content available to preview)",
            dim,
        )));
    } else {
        let shown = facts.preview.len();
        let total = facts.preview_total.max(shown as u64);
        lines.push(Line::from(Span::styled(
            if (shown as u64) < total {
                format!("first {shown} of {total} bytes")
            } else {
                format!("all {total} bytes")
            },
            dim,
        )));
        lines.extend(
            hex_dump(facts.preview)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, dim))),
        );
    }

    lines.push(Line::default());
    BinaryBlock {
        lines,
        panes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_identify_image_formats_from_magic_bytes() {
        // given
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";
        let webp = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

        // when / then
        assert_eq!(detect_kind(png), FileKind::Image("PNG image"));
        assert_eq!(detect_kind(webp), FileKind::Image("WebP image"));
        assert_eq!(detect_kind(b"%PDF-1.4"), FileKind::Other("PDF document"));
        assert_eq!(detect_kind(b"nothing here"), FileKind::Unknown);
    }

    #[test]
    fn should_not_read_webp_marker_past_a_short_riff_header() {
        // given a RIFF file truncated before the format marker
        let truncated = b"RIFF\x24\x00\x00";

        // when
        let kind = detect_kind(truncated);

        // then
        assert_eq!(kind, FileKind::Unknown);
    }

    #[test]
    fn should_prefer_magic_bytes_over_a_lying_extension() {
        // given a path named .png holding JPEG bytes
        let path = Path::new("logo.png");
        let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF";

        // when
        let kind = detect_kind(bytes);

        // then
        assert!(has_image_extension(path));
        assert_eq!(kind, FileKind::Image("JPEG image"));
    }

    #[test]
    fn should_format_hex_dump_rows_in_xxd_columns() {
        // given
        let bytes: Vec<u8> = (0u8..20).collect();

        // when
        let rows = hex_dump(&bytes);

        // then
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            "00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f  |................|"
        );
        assert_eq!(
            rows[1],
            "00000010  10 11 12 13                                       |....|"
        );
    }

    #[test]
    fn should_render_printable_ascii_in_the_hex_gutter() {
        // given
        let bytes = b"Hi there!\n";

        // when
        let rows = hex_dump(bytes);

        // then
        assert!(rows[0].ends_with("|Hi there!.|"), "got {}", rows[0]);
    }

    #[test]
    fn should_cap_the_hex_dump_at_the_preview_limit() {
        // given
        let bytes = vec![0u8; HEX_PREVIEW_BYTES * 3];

        // when
        let rows = hex_dump(&bytes);

        // then
        assert_eq!(rows.len(), HEX_PREVIEW_BYTES / HEX_BYTES_PER_ROW);
    }

    #[test]
    fn should_report_size_delta_only_when_both_sides_exist() {
        // given / when / then
        assert_eq!(
            format_size_delta(Some(1024), Some(2048)).as_deref(),
            Some("+1.0 KiB")
        );
        assert_eq!(
            format_size_delta(Some(2048), Some(1024)).as_deref(),
            Some("-1.0 KiB")
        );
        assert_eq!(
            format_size_delta(Some(10), Some(10)).as_deref(),
            Some("no size change")
        );
        assert_eq!(format_size_delta(None, Some(10)), None);
    }

    #[test]
    fn should_format_sizes_with_binary_prefixes() {
        // given / when / then
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MiB");
    }

    fn image_facts() -> BinaryFacts<'static> {
        BinaryFacts {
            old: SideFacts {
                size: Some(1000),
                kind: Some(FileKind::Image("PNG image")),
                dimensions: Some((32, 32)),
                lfs: None,
            },
            new: SideFacts {
                size: Some(2000),
                kind: Some(FileKind::Image("PNG image")),
                dimensions: Some((64, 64)),
                lfs: None,
            },
            preview: &[],
            preview_total: 0,
            loading: false,
        }
    }

    #[test]
    fn should_lay_image_panes_side_by_side_when_wide() {
        // given
        let theme = Theme::default();

        // when
        let block = binary_block(&theme, &image_facts(), 80, 40);

        // then
        assert_eq!(block.panes.len(), 2);
        assert!(block.panes.iter().all(|p| p.row == 1));
        assert_eq!(block.panes[0].column, 0);
        assert_eq!(block.panes[1].column, 40);
        assert_eq!(block.panes[0].width, 39);
    }

    #[test]
    fn should_stack_image_panes_when_narrow() {
        // given
        let theme = Theme::default();

        // when
        let block = binary_block(&theme, &image_facts(), 30, 60);

        // then
        assert_eq!(block.panes.len(), 2);
        assert!(block.panes[0].row < block.panes[1].row);
        assert!(block.panes.iter().all(|p| p.column == 0));
        assert!(block.panes.iter().all(|p| p.width == 30));
    }

    #[test]
    fn should_keep_an_image_block_within_a_short_viewport() {
        // given a viewport far shorter than the preferred image height
        let theme = Theme::default();

        // when
        let block = binary_block(&theme, &image_facts(), 80, 10);

        // then
        assert!(
            block.lines.len() <= 10,
            "block of {} rows must fit a 10-row viewport",
            block.lines.len()
        );
    }

    #[test]
    fn should_reserve_exactly_one_pane_for_an_added_image() {
        // given an added file: no old side
        let mut facts = image_facts();
        facts.old = SideFacts::default();
        let theme = Theme::default();

        // when
        let block = binary_block(&theme, &facts, 80, 40);

        // then
        assert_eq!(block.panes.len(), 1);
        assert_eq!(block.panes[0].side, LineSide::New);
    }

    /// The reviewer needs to know the image is missing and how to get it,
    /// not read a hex dump of a pointer.
    #[test]
    fn should_name_the_missing_lfs_object_and_the_command_that_fetches_it() {
        // given a new side whose LFS object was never fetched
        let theme = Theme::default();
        let facts = BinaryFacts {
            old: SideFacts::default(),
            new: SideFacts {
                size: Some(3 * 1024 * 1024),
                kind: Some(FileKind::Other("Git LFS object")),
                dimensions: None,
                lfs: Some(LfsNotice {
                    oid: "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
                        .to_string(),
                    reason: LfsMissing::NotFetched,
                }),
            },
            preview: &[],
            preview_total: 0,
            loading: false,
        };

        // when
        let block = binary_block(&theme, &facts, 80, 40);

        // then
        let rendered: Vec<String> = block.lines.iter().map(Line::to_string).collect();
        assert!(
            block.panes.is_empty(),
            "no bytes means no image pane to paint"
        );
        assert!(
            rendered.iter().any(|line| line
                == "LFS object not present locally (oid 4d7a214614ab…, 3.0 MiB) — run `git lfs fetch`"),
            "card must name the object and the fix, got {rendered:?}"
        );
    }

    #[test]
    fn should_render_a_metadata_card_with_a_hex_dump_for_non_images() {
        // given
        let theme = Theme::default();
        let preview = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n";
        let facts = BinaryFacts {
            old: SideFacts {
                size: Some(100),
                kind: Some(FileKind::Other("PDF document")),
                dimensions: None,
                lfs: None,
            },
            new: SideFacts {
                size: Some(220),
                kind: Some(FileKind::Other("PDF document")),
                dimensions: None,
                lfs: None,
            },
            preview,
            preview_total: 220,
            loading: false,
        };

        // when
        let block = binary_block(&theme, &facts, 80, 40);
        let text: Vec<String> = block.lines.iter().map(|l| l.to_string()).collect();

        // then
        assert!(block.panes.is_empty());
        assert!(text[0].contains("PDF document"));
        assert!(text.iter().any(|l| l.contains("+120 B")));
        assert!(text.iter().any(|l| l.contains("|%PDF-1.4.%.....|")));
    }
}
