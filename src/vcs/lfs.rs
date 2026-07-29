//! Git LFS pointer detection and content resolution.
//!
//! An LFS-tracked file's blob is a three-line pointer, not the media, so every
//! read of a "binary" file has to ask whether it got a pointer and, if so,
//! resolve the real object. Resolution prefers the repository's local object
//! store and only shells out to `git lfs` when that misses.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{DiffHunk, LineOrigin};

/// Largest LFS object we will load. Above this the metadata card stands in:
/// the pointer's `size` field is read before any bytes, so an oversized object
/// is never fetched or held.
pub const MAX_RESOLVED_SIZE: u64 = 50 * 1024 * 1024;

/// The pointer format caps a pointer file at a few hundred bytes; anything
/// larger is a real file that happens to start with the version line.
const MAX_POINTER_BYTES: usize = 1024;

const POINTER_VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1";

/// A parsed git-LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// The sha256 digest, without the `sha256:` prefix.
    pub oid: String,
    /// Size of the real content in bytes.
    pub size: u64,
}

/// Why an LFS object's content is not on offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsMissing {
    /// The object is not in the local store and `git lfs` could not supply it.
    NotFetched,
    /// The pointer declares a size past [`MAX_RESOLVED_SIZE`].
    TooLarge,
}

/// What a read of a file's raw bytes produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileBytes {
    Bytes(Vec<u8>),
    /// An LFS pointer whose real content we cannot show.
    Lfs {
        pointer: Pointer,
        reason: LfsMissing,
    },
}

impl FileBytes {
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            FileBytes::Bytes(bytes) => Some(bytes),
            FileBytes::Lfs { .. } => None,
        }
    }
}

/// Where a repository keeps its LFS objects.
pub struct Store<'a> {
    /// The common `.git` directory. Worktrees share one object store, so this
    /// must be the common dir, not the per-worktree gitdir.
    pub common_dir: &'a Path,
    /// Working tree root, used as the cwd for `git lfs`.
    pub workdir: Option<&'a Path>,
}

/// Parse a git-LFS pointer, strictly.
///
/// Returns `None` for anything that is not exactly a pointer: the first line
/// must be the version line, and both `oid sha256:<64 hex>` and `size <int>`
/// must be present and well formed.
pub fn parse_pointer(bytes: &[u8]) -> Option<Pointer> {
    if bytes.len() > MAX_POINTER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != POINTER_VERSION_LINE {
        return None;
    }

    let mut oid = None;
    let mut size = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(' ')?;
        match key {
            "oid" => oid = Some(parse_oid(value)?),
            "size" => size = Some(value.parse::<u64>().ok()?),
            // The spec allows further keys (`ext-0-...`), which we ignore.
            _ => {}
        }
    }

    Some(Pointer {
        oid: oid?,
        size: size?,
    })
}

fn parse_oid(value: &str) -> Option<String> {
    let hex = value.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_string())
}

/// Path of an object in the repository's local LFS store.
pub fn object_path(common_dir: &Path, oid: &str) -> PathBuf {
    common_dir
        .join("lfs")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

/// Turn raw blob bytes into the content to display.
///
/// Bytes that are not a pointer pass through untouched, so every caller can
/// route its reads through here.
pub fn resolve(bytes: Vec<u8>, store: &Store<'_>) -> FileBytes {
    let Some(pointer) = parse_pointer(&bytes) else {
        return FileBytes::Bytes(bytes);
    };
    if pointer.size > MAX_RESOLVED_SIZE {
        return FileBytes::Lfs {
            pointer,
            reason: LfsMissing::TooLarge,
        };
    }
    if let Some(content) = read_local_object(store.common_dir, &pointer) {
        return FileBytes::Bytes(content);
    }
    if let Some(workdir) = store.workdir
        && let Some(content) = smudge(workdir, &bytes)
    {
        return FileBytes::Bytes(content);
    }
    FileBytes::Lfs {
        pointer,
        reason: LfsMissing::NotFetched,
    }
}

fn read_local_object(common_dir: &Path, pointer: &Pointer) -> Option<Vec<u8>> {
    let content = std::fs::read(object_path(common_dir, &pointer.oid)).ok()?;
    // A short read means a partial download; the pointer's size is the truth.
    (content.len() as u64 == pointer.size).then_some(content)
}

/// Ask `git lfs` for the content the pointer names.
///
/// Only reached when the object is not in the local store, so this may go to
/// the network; it fails quietly when git-lfs is not installed.
fn smudge(workdir: &Path, pointer_bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;

    let mut child = Command::new("git")
        .current_dir(workdir)
        .args(["lfs", "smudge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(pointer_bytes).ok()?;
    let output = child.wait_with_output().ok()?;
    // git-lfs echoes the pointer straight back when it cannot get the object.
    if !output.status.success() || parse_pointer(&output.stdout).is_some() {
        return None;
    }
    Some(output.stdout)
}

/// Whether a parsed text diff is really a diff of an LFS pointer.
///
/// Backends see the pointer, not the media, so an LFS image arrives as a small
/// text change. A pointer is three short lines, so one hunk holds the whole
/// file on each side; reconstructing the sides and parsing strictly is enough
/// to tell a pointer from a file that merely mentions one.
pub fn hunks_are_pointer(hunks: &[DiffHunk]) -> bool {
    let [hunk] = hunks else {
        return false;
    };
    [LineOrigin::Deletion, LineOrigin::Addition]
        .into_iter()
        .any(|changed| {
            let side: String = hunk
                .lines
                .iter()
                .filter(|line| line.origin == changed || line.origin == LineOrigin::Context)
                .flat_map(|line| [line.content.as_str(), "\n"])
                .collect();
            parse_pointer(side.as_bytes()).is_some()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    fn pointer_text(size: u64) -> String {
        format!("{POINTER_VERSION_LINE}\noid sha256:{OID}\nsize {size}\n")
    }

    #[test]
    fn should_parse_a_well_formed_pointer() {
        // given
        let text = pointer_text(12_345);

        // when
        let parsed = parse_pointer(text.as_bytes());

        // then
        assert_eq!(
            parsed,
            Some(Pointer {
                oid: OID.to_string(),
                size: 12_345,
            })
        );
    }

    #[test]
    fn should_reject_content_that_is_not_a_pointer() {
        // given
        let truncated = format!("{POINTER_VERSION_LINE}\noid sha256:{OID}\n");
        let short_oid = format!("{POINTER_VERSION_LINE}\noid sha256:abc123\nsize 12\n");
        let bad_size = format!("{POINTER_VERSION_LINE}\noid sha256:{OID}\nsize huge\n");
        let mentions_the_spec =
            format!("The pointer starts with\n{POINTER_VERSION_LINE}\nand two more lines.\n");

        // when / then
        assert_eq!(parse_pointer(truncated.as_bytes()), None, "no size line");
        assert_eq!(parse_pointer(short_oid.as_bytes()), None, "oid too short");
        assert_eq!(
            parse_pointer(bad_size.as_bytes()),
            None,
            "size not a number"
        );
        assert_eq!(
            parse_pointer(mentions_the_spec.as_bytes()),
            None,
            "version line must come first"
        );
        assert_eq!(parse_pointer(b"\x89PNG\r\n\x1a\n"), None, "binary content");
        assert_eq!(parse_pointer(b""), None, "empty content");
    }

    #[test]
    fn should_reject_a_file_that_only_opens_like_a_pointer() {
        // given a real file whose first line matches, padded past the cap
        let mut text = pointer_text(1);
        text.push_str(&"filler filler filler\n".repeat(80));

        // when / then
        assert_eq!(parse_pointer(text.as_bytes()), None);
    }

    #[test]
    fn should_shard_the_object_path_by_the_first_two_oid_byte_pairs() {
        // given
        let common_dir = Path::new("/repo/.git");

        // when
        let path = object_path(common_dir, OID);

        // then
        assert_eq!(
            path,
            Path::new("/repo/.git/lfs/objects/4d/7a").join(OID),
            "LFS shards on the first two byte pairs of the oid"
        );
    }

    #[test]
    fn should_report_an_oversized_object_without_touching_the_store() {
        // given a pointer past the cap and a store that holds nothing
        let temp = tempfile::tempdir().expect("temp dir");
        let store = Store {
            common_dir: temp.path(),
            workdir: None,
        };
        let text = pointer_text(MAX_RESOLVED_SIZE + 1);

        // when
        let resolved = resolve(text.into_bytes(), &store);

        // then
        assert_eq!(
            resolved,
            FileBytes::Lfs {
                pointer: Pointer {
                    oid: OID.to_string(),
                    size: MAX_RESOLVED_SIZE + 1,
                },
                reason: LfsMissing::TooLarge,
            }
        );
    }

    #[test]
    fn should_read_the_object_from_the_local_store() {
        // given an object placed where LFS keeps it
        let temp = tempfile::tempdir().expect("temp dir");
        let content = b"\x89PNG\r\n\x1a\nreal image bytes".to_vec();
        let path = object_path(temp.path(), OID);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &content).unwrap();
        let store = Store {
            common_dir: temp.path(),
            workdir: None,
        };

        // when
        let resolved = resolve(pointer_text(content.len() as u64).into_bytes(), &store);

        // then
        assert_eq!(resolved, FileBytes::Bytes(content));
    }

    #[test]
    fn should_report_a_missing_object_rather_than_the_pointer_text() {
        // given an empty store and no working tree to smudge in
        let temp = tempfile::tempdir().expect("temp dir");
        let store = Store {
            common_dir: temp.path(),
            workdir: None,
        };

        // when
        let resolved = resolve(pointer_text(64).into_bytes(), &store);

        // then
        assert_eq!(
            resolved,
            FileBytes::Lfs {
                pointer: Pointer {
                    oid: OID.to_string(),
                    size: 64,
                },
                reason: LfsMissing::NotFetched,
            }
        );
    }

    #[test]
    fn should_pass_ordinary_bytes_through() {
        // given
        let temp = tempfile::tempdir().expect("temp dir");
        let store = Store {
            common_dir: temp.path(),
            workdir: None,
        };
        let bytes = b"\x89PNG\r\n\x1a\n".to_vec();

        // when
        let resolved = resolve(bytes.clone(), &store);

        // then
        assert_eq!(resolved, FileBytes::Bytes(bytes));
    }
}
