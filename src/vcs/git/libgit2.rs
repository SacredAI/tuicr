use git2::Repository;
use std::path::{Path, PathBuf};
use std::sync::Once;

use crate::error::{Result, TuicrError};
use crate::model::{DiffFile, DiffLine, FileStatus};
use crate::syntax::SyntaxHighlighter;

use super::{context, diff, repository, staging};
use crate::vcs::lfs::FileBytes;
use crate::vcs::traits::{
    ChangeKind, CommitInfo, DiffWhitespaceMode, ResolvedRevisionRange, VcsBackend, VcsInfo, VcsType,
};

/// Git backend implementation using the git2/libgit2 library.
pub struct Libgit2Backend {
    repo: Repository,
    info: VcsInfo,
    whitespace_mode: DiffWhitespaceMode,
}

/// Declare libgit2 extensions tuicr understands so discovery doesn't refuse
/// repos that opt into newer git on-disk features.
///
/// Currently: `relativeworktrees` (git 2.48+ `worktree.useRelativePaths`).
/// Without this declaration libgit2 refuses to open a worktree created from a
/// bare clone with that setting — tuicr would surface as "Not a repository"
/// while plain `git status` works fine. Path resolution for relative
/// `gitdir:` pointers already works in libgit2; only the safety gate refuses.
fn register_supported_extensions() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: libgit2 stores extensions in a process-wide static. We call
        // this exactly once, before any `Repository::discover`, via `Once`.
        unsafe {
            let _ = git2::opts::set_extensions(&["relativeworktrees"]);
        }
    });
}

impl Libgit2Backend {
    pub(super) fn discover_from(cwd: &Path, whitespace_mode: DiffWhitespaceMode) -> Result<Self> {
        register_supported_extensions();
        let repo = Repository::discover(cwd).map_err(|_| TuicrError::NotARepository)?;

        let root_path = repo
            .workdir()
            .ok_or(TuicrError::NotARepository)?
            .to_path_buf();

        let head_commit = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .map(|c| c.id().to_string())
            .unwrap_or_else(|| "HEAD".to_string());

        // For unborn HEAD (fresh `git init` / `git clone` of an empty remote),
        // `repo.head()` errors, so fall back to reading HEAD's symbolic target
        // directly. That way the status bar still shows e.g. `git:main` instead
        // of `git:detached` before the first commit lands.
        let branch_name = repo
            .head()
            .ok()
            .and_then(|h| {
                if h.is_branch() {
                    h.shorthand().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .or_else(|| {
                repo.find_reference("HEAD")
                    .ok()
                    .and_then(|r| r.symbolic_target().map(str::to_string))
                    .and_then(|t| t.strip_prefix("refs/heads/").map(str::to_string))
            });

        let info = VcsInfo {
            root_path,
            head_commit,
            branch_name,
            vcs_type: VcsType::Git,
        };

        Ok(Self {
            repo,
            info,
            whitespace_mode,
        })
    }
}

impl VcsBackend for Libgit2Backend {
    fn info(&self) -> &VcsInfo {
        &self.info
    }

    fn supports_sparse_checkout(&self) -> bool {
        false
    }

    fn get_working_tree_diff(&self, highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        diff::get_working_tree_diff(&self.repo, self.whitespace_mode, highlighter)
    }

    fn get_staged_diff(&self, highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        diff::get_staged_diff(&self.repo, self.whitespace_mode, highlighter)
    }

    fn get_unstaged_diff(&self, highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        diff::get_unstaged_diff(&self.repo, self.whitespace_mode, highlighter)
    }

    fn list_changed_paths(&self, kind: ChangeKind) -> Result<Vec<PathBuf>> {
        diff::list_changed_paths(&self.repo, kind)
    }

    fn fetch_context_lines(
        &self,
        file_path: &Path,
        file_status: FileStatus,
        ref_commit: Option<&str>,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        context::fetch_context_lines(
            &self.repo,
            file_path,
            file_status,
            ref_commit,
            start_line,
            end_line,
        )
    }

    fn file_line_count(
        &self,
        file_path: &Path,
        file_status: FileStatus,
        ref_commit: Option<&str>,
    ) -> Result<u32> {
        context::file_line_count(&self.repo, file_path, file_status, ref_commit)
    }

    fn read_file_bytes(&self, file_path: &Path, rev: Option<&str>) -> Result<Option<FileBytes>> {
        context::read_file_bytes(&self.repo, file_path, rev)
    }

    fn get_recent_commits(&self, offset: usize, limit: usize) -> Result<Vec<CommitInfo>> {
        let git_commits = repository::get_recent_commits(&self.repo, offset, limit)?;
        Ok(git_commits
            .into_iter()
            .map(|c| CommitInfo {
                id: c.id,
                short_id: c.short_id,
                branch_name: c.branch_name,
                summary: c.summary,
                body: c.body,
                author: c.author,
                time: c.time,
            })
            .collect())
    }

    fn resolve_revision_range(&self, revisions: &str) -> Result<ResolvedRevisionRange<'static>> {
        repository::resolve_revision_range(&self.repo, revisions)
    }

    fn get_commit_range_diff(
        &self,
        revision_range: &ResolvedRevisionRange<'_>,
        highlighter: &SyntaxHighlighter,
    ) -> Result<Vec<DiffFile>> {
        diff::get_commit_range_diff(
            &self.repo,
            revision_range,
            self.whitespace_mode,
            highlighter,
        )
    }

    fn get_commits_info(&self, ids: &[String]) -> Result<Vec<CommitInfo>> {
        let git_commits = repository::get_commits_info(&self.repo, ids)?;
        Ok(git_commits
            .into_iter()
            .map(|c| CommitInfo {
                id: c.id,
                short_id: c.short_id,
                branch_name: c.branch_name,
                summary: c.summary,
                body: c.body,
                author: c.author,
                time: c.time,
            })
            .collect())
    }

    fn get_working_tree_with_commits_diff(
        &self,
        commit_ids: &[String],
        highlighter: &SyntaxHighlighter,
    ) -> Result<Vec<DiffFile>> {
        diff::get_working_tree_with_commits_diff(
            &self.repo,
            commit_ids,
            self.whitespace_mode,
            highlighter,
        )
    }

    fn stage_file(&self, path: &Path) -> Result<()> {
        staging::stage_file(&self.repo, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(workdir: &Path, args: &[&str]) {
        // `-c commit.gpgsign=false` overrides any global signing config so
        // contributors with commit signing enabled aren't prompted to sign
        // throwaway commits in these temp repos.
        // `-c safe.bareRepository=all` allows commands run inside bare test
        // repositories to work on systems with `safe.bareRepository=explicit`.
        let output = Command::new("git")
            .current_dir(workdir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultRefFormat=files",
                "-c",
                "safe.bareRepository=all",
            ])
            .args(args)
            .output()
            .expect("failed to run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn should_discover_worktree_with_relativeworktrees_extension() {
        // given: a bare clone with `extensions.relativeworktrees = true` set
        // and a worktree linked to it. This is the on-disk state git 2.48+
        // produces with `worktree.useRelativePaths`. Without
        // `set_extensions(["relativeworktrees"])` libgit2 refuses to open the
        // worktree at all, surfacing as "Not a repository" in tuicr.
        let temp = tempfile::tempdir().expect("temp dir");
        let source = temp.path().join("source");
        let bare = temp.path().join("bare.git");
        let worktree = temp.path().join("wt");

        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        git(&source, &["config", "user.name", "Test User"]);
        fs::write(source.join("README"), "hello\n").unwrap();
        git(&source, &["add", "README"]);
        git(&source, &["commit", "-q", "-m", "init"]);

        git(
            temp.path(),
            &[
                "clone",
                "--bare",
                "-q",
                source.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        // The extension declaration is the gate libgit2 enforces. Setting
        // this alone reproduces the failure regardless of the local git
        // version — `worktree.useRelativePaths` (git 2.48+) is what writes
        // this in real-world setups. `core.repositoryFormatVersion = 1` is
        // required to opt into the `extensions.*` namespace at all.
        git(&bare, &["config", "core.repositoryFormatVersion", "1"]);
        git(&bare, &["config", "extensions.relativeworktrees", "true"]);
        git(
            &bare,
            &["worktree", "add", "-q", worktree.to_str().unwrap()],
        );

        // when
        let backend = Libgit2Backend::discover_from(&worktree, DiffWhitespaceMode::Normal)
            .expect("worktree with relativeworktrees extension should open");

        // then
        assert_eq!(backend.info().vcs_type, VcsType::Git);
        assert!(
            backend.repo.workdir().is_some(),
            "worktree must report a workdir"
        );
    }

    /// The whole point of the `read_file_bytes` seam: every other content read
    /// in this backend goes through `str::from_utf8` or a lossy conversion,
    /// either of which would destroy the image it is meant to display.
    #[test]
    fn should_read_binary_bytes_verbatim_from_both_sides_of_a_diff() {
        // given a committed blob with non-UTF-8 bytes, replaced in the worktree
        let temp = tempfile::tempdir().expect("temp dir");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        git(&repo_path, &["init", "-q", "-b", "main"]);
        git(&repo_path, &["config", "user.email", "test@example.com"]);
        git(&repo_path, &["config", "user.name", "Test User"]);

        let committed: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe, 0x80];
        let working: Vec<u8> = vec![0xff, 0xd8, 0xff, 0x00, 0x01, 0x02];
        fs::write(repo_path.join("logo.png"), &committed).unwrap();
        git(&repo_path, &["add", "logo.png"]);
        git(&repo_path, &["commit", "-q", "-m", "add logo"]);
        fs::write(repo_path.join("logo.png"), &working).unwrap();

        let backend = Libgit2Backend::discover_from(&repo_path, DiffWhitespaceMode::Normal)
            .expect("repo should open");
        let path = Path::new("logo.png");

        // when
        let old = backend.read_file_bytes(path, Some("HEAD")).unwrap();
        let new = backend.read_file_bytes(path, None).unwrap();
        let missing = backend.read_file_bytes(Path::new("absent.png"), Some("HEAD"));

        // then
        assert_eq!(
            old.as_ref().and_then(FileBytes::bytes),
            Some(committed.as_slice())
        );
        assert_eq!(
            new.as_ref().and_then(FileBytes::bytes),
            Some(working.as_slice())
        );
        assert_eq!(
            missing.unwrap(),
            None,
            "a path absent at the revision is an empty side, not an error"
        );
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test User"]);
    }

    fn pointer_for(oid_seed: char, size: u64) -> String {
        let oid: String = std::iter::repeat_n(oid_seed, 64).collect();
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
    }

    fn store_lfs_object(repo_path: &Path, oid_seed: char, content: &[u8]) {
        let oid: String = std::iter::repeat_n(oid_seed, 64).collect();
        let path = crate::vcs::lfs::object_path(&repo_path.join(".git"), &oid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// An LFS-tracked image must reach the image view as the image, on both
    /// sides, without git-lfs being installed: the objects are already local.
    #[test]
    fn should_resolve_lfs_pointers_to_the_objects_in_the_local_store() {
        // given a committed pointer and a different pointer in the worktree,
        // each with its object present in .git/lfs/objects
        let temp = tempfile::tempdir().expect("temp dir");
        let repo_path = temp.path().join("repo");
        init_repo(&repo_path);

        let committed = b"\x89PNG\r\n\x1a\nold image".to_vec();
        let working = b"\x89PNG\r\n\x1a\nnew image, longer".to_vec();
        store_lfs_object(&repo_path, 'a', &committed);
        store_lfs_object(&repo_path, 'b', &working);

        fs::write(
            repo_path.join("logo.png"),
            pointer_for('a', committed.len() as u64),
        )
        .unwrap();
        git(&repo_path, &["add", "logo.png"]);
        git(&repo_path, &["commit", "-q", "-m", "add logo"]);
        fs::write(
            repo_path.join("logo.png"),
            pointer_for('b', working.len() as u64),
        )
        .unwrap();

        let backend = Libgit2Backend::discover_from(&repo_path, DiffWhitespaceMode::Normal)
            .expect("repo should open");
        let path = Path::new("logo.png");

        // when
        let old = backend.read_file_bytes(path, Some("HEAD")).unwrap();
        let new = backend.read_file_bytes(path, None).unwrap();

        // then
        assert_eq!(
            old.as_ref().and_then(FileBytes::bytes),
            Some(committed.as_slice())
        );
        assert_eq!(
            new.as_ref().and_then(FileBytes::bytes),
            Some(working.as_slice())
        );
    }

    /// Without the object, the pointer text is worthless to a reviewer; the
    /// backend has to say so rather than hand back 130 bytes of metadata.
    #[test]
    fn should_report_an_unfetched_lfs_object_instead_of_the_pointer_text() {
        // given a committed pointer whose object was never fetched
        let temp = tempfile::tempdir().expect("temp dir");
        let repo_path = temp.path().join("repo");
        init_repo(&repo_path);
        fs::write(repo_path.join("logo.png"), pointer_for('c', 4096)).unwrap();
        git(&repo_path, &["add", "logo.png"]);
        git(&repo_path, &["commit", "-q", "-m", "add logo"]);

        let backend = Libgit2Backend::discover_from(&repo_path, DiffWhitespaceMode::Normal)
            .expect("repo should open");

        // when
        let read = backend
            .read_file_bytes(Path::new("logo.png"), Some("HEAD"))
            .unwrap();

        // then
        assert!(
            matches!(
                read,
                Some(FileBytes::Lfs {
                    reason: crate::vcs::lfs::LfsMissing::NotFetched,
                    ref pointer,
                }) if pointer.size == 4096
            ),
            "an absent object must report itself, got {read:?}"
        );
    }

    /// Git calls a pointer file text, so without this the reviewer reads a
    /// three-line hash diff where an image belongs.
    #[test]
    fn should_classify_a_changed_lfs_pointer_as_a_binary_file() {
        // given a committed pointer replaced by another in the worktree
        let temp = tempfile::tempdir().expect("temp dir");
        let repo_path = temp.path().join("repo");
        init_repo(&repo_path);
        fs::write(repo_path.join("logo.png"), pointer_for('a', 10)).unwrap();
        fs::write(repo_path.join("notes.md"), "version notes\nsecond line\n").unwrap();
        git(&repo_path, &["add", "."]);
        git(&repo_path, &["commit", "-q", "-m", "add files"]);
        fs::write(repo_path.join("logo.png"), pointer_for('b', 20)).unwrap();
        fs::write(repo_path.join("notes.md"), "version notes\nthird line\n").unwrap();

        let backend = Libgit2Backend::discover_from(&repo_path, DiffWhitespaceMode::Normal)
            .expect("repo should open");

        // when
        let files = backend
            .get_working_tree_diff(&SyntaxHighlighter::default())
            .expect("diff should parse");

        // then
        let pointer_file = files
            .iter()
            .find(|f| f.display_path().ends_with("logo.png"))
            .expect("pointer file in diff");
        assert!(pointer_file.is_binary, "an LFS pointer diff is binary");
        assert!(
            pointer_file.hunks.is_empty(),
            "a binary file shows no text hunks"
        );
        let text_file = files
            .iter()
            .find(|f| f.display_path().ends_with("notes.md"))
            .expect("text file in diff");
        assert!(!text_file.is_binary, "ordinary text stays text");
    }
}
