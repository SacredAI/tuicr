//! Headless render-path timing harness.
//!
//! Not part of the test suite's correctness gate: every entry point is
//! `#[ignore]`d and only runs on demand via
//! `cargo test --release perf_bench -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::{App, DiffSource, InputMode};
use crate::error::{Result as TuicrResult, TuicrError};
use crate::model::SessionDiffSource;
use crate::model::diff_types::{DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin};
use crate::model::review::ReviewSession;
use crate::syntax::SyntaxHighlighter;
use crate::theme::Theme;
use crate::vcs::traits::{VcsBackend, VcsChangeStatus, VcsInfo, VcsType};

const WIDTH: u16 = 200;
const HEIGHT: u16 = 50;

struct StubVcs {
    info: VcsInfo,
}

impl VcsBackend for StubVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
    }
    fn get_working_tree_diff(&self, _h: &SyntaxHighlighter) -> TuicrResult<Vec<DiffFile>> {
        Err(TuicrError::NoChanges)
    }
    fn fetch_context_lines(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
        _start_line: u32,
        _end_line: u32,
    ) -> TuicrResult<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn get_change_status(&self) -> TuicrResult<VcsChangeStatus> {
        Ok(VcsChangeStatus {
            staged: false,
            unstaged: false,
        })
    }
    fn file_line_count(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> TuicrResult<u32> {
        Ok(0)
    }
}

/// Deterministic pseudo-random source so fixture shape is stable run to run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn source_line(rng: &mut Rng, i: usize) -> String {
    match rng.below(10) {
        0 => format!(
            "    // a fairly long explanatory comment about item {i} that runs past a typical terminal width to exercise the wrapping and truncation paths"
        ),
        1 => format!(
            "    let value_{i} = compute_something(&context, {i}, \"literal string {i}\")?;"
        ),
        2 => format!("        if condition_{i} && other_condition {{"),
        3 => String::new(),
        4 => format!("    pub fn handler_{i}(&mut self, request: &Request) -> Result<Response> {{"),
        5 => format!("            map.insert(\"key_{i}\".to_string(), Value::Number({i}.into()));"),
        6 => format!(
            "    const VERY_LONG_CONSTANT_NAME_FOR_ITEM_{i}: &str = \"{}\";",
            "x".repeat(180)
        ),
        7 => "        }".to_string(),
        8 => format!("    #[derive(Debug, Clone, PartialEq)] struct Item{i} {{ field: u64 }}"),
        _ => format!("    process(item_{i});"),
    }
}

/// Build a synthetic diff of `files` files, each with `hunks_per_file` hunks of
/// `lines_per_hunk` lines, syntax-highlighted the same way the real VCS layer
/// highlights at load time.
fn build_fixture(files: usize, hunks_per_file: usize, lines_per_hunk: usize) -> Vec<DiffFile> {
    let highlighter = SyntaxHighlighter::default();
    let mut rng = Rng(0x2545F491_4F6CDD1D);
    let mut out = Vec::with_capacity(files);

    for f in 0..files {
        let path = PathBuf::from(format!("src/module_{}/component_{f}.rs", f % 17));
        let mut hunks = Vec::with_capacity(hunks_per_file);

        for h in 0..hunks_per_file {
            let mut contents = Vec::with_capacity(lines_per_hunk);
            let mut origins = Vec::with_capacity(lines_per_hunk);
            for l in 0..lines_per_hunk {
                contents.push(source_line(&mut rng, f * 1000 + h * 100 + l));
                origins.push(match rng.below(4) {
                    0 => LineOrigin::Addition,
                    1 => LineOrigin::Deletion,
                    _ => LineOrigin::Context,
                });
            }

            let seq = SyntaxHighlighter::split_diff_lines_for_highlighting(&contents, &origins);
            let old_hl = highlighter.highlight_file_lines(&path, &seq.old_lines);
            let new_hl = highlighter.highlight_file_lines(&path, &seq.new_lines);

            let start = (h * lines_per_hunk + 1) as u32;
            let mut old_no = start;
            let mut new_no = start;
            let mut lines = Vec::with_capacity(lines_per_hunk);
            for (i, content) in contents.iter().enumerate() {
                let origin = origins[i];
                let highlighted_spans = highlighter.highlighted_line_for_diff_with_background(
                    old_hl.as_deref(),
                    new_hl.as_deref(),
                    seq.old_line_indices[i],
                    seq.new_line_indices[i],
                    origin,
                );
                let (old_lineno, new_lineno) = match origin {
                    LineOrigin::Context => {
                        let v = (Some(old_no), Some(new_no));
                        old_no += 1;
                        new_no += 1;
                        v
                    }
                    LineOrigin::Addition => {
                        let v = (None, Some(new_no));
                        new_no += 1;
                        v
                    }
                    LineOrigin::Deletion => {
                        let v = (Some(old_no), None);
                        old_no += 1;
                        v
                    }
                };
                lines.push(DiffLine {
                    origin,
                    content: content.clone(),
                    old_lineno,
                    new_lineno,
                    highlighted_spans,
                    intraline: Vec::new(),
                });
            }

            hunks.push(DiffHunk {
                header: format!("@@ -{start},{lines_per_hunk} +{start},{lines_per_hunk} @@"),
                lines,
                old_start: start,
                old_count: lines_per_hunk as u32,
                new_start: start,
                new_count: lines_per_hunk as u32,
                needs_highlight: true,
            });
        }

        let content_hash = DiffFile::compute_content_hash(&hunks);
        out.push(DiffFile {
            old_path: Some(path.clone()),
            new_path: Some(path),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        });
    }

    out
}

fn build_app(diff_files: Vec<DiffFile>) -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp/tuicr-perf"),
        head_commit: "headsha".to_string(),
        branch_name: None,
        vcs_type: VcsType::Git,
    };
    let session = ReviewSession::new(
        vcs_info.root_path.clone(),
        "headsha".to_string(),
        None,
        SessionDiffSource::CommitRange,
    );
    App::build(
        Box::new(StubVcs {
            info: vcs_info.clone(),
        }),
        vcs_info,
        Theme::dark(),
        None,
        false,
        diff_files,
        session,
        DiffSource::CommitRange(vec!["HEAD".to_string()]),
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("build app")
}

struct Stats {
    median: Duration,
    p95: Duration,
}

fn stats(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    Stats { median, p95 }
}

/// Render `iters` frames, calling `step` between each, and report frame time.
fn time_frames(label: &str, app: &mut App, iters: usize, mut step: impl FnMut(&mut App)) -> Stats {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal");

    // Warm up so first-frame lazy init doesn't skew the sample.
    for _ in 0..3 {
        terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
    }

    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        step(app);
        let t = Instant::now();
        terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
        samples.push(t.elapsed());
    }

    let s = stats(samples);
    println!("{label:<28} median {:>9.3?}  p95 {:>9.3?}", s.median, s.p95);
    s
}

/// Frame-time battery run against a fully built app.
fn frame_battery(app: &mut App) {
    time_frames("idle redraw", app, 50, |_| {});
    time_frames("scroll by one", app, 200, |a| a.cursor_down(1));
    time_frames("page down", app, 100, |a| a.cursor_down(HEIGHT as usize));

    app.jump_to_file(0);
    time_frames("next file", app, 50, |a| a.next_file());

    app.cursor_to_top();
    time_frames("search next", app, 50, |a| {
        a.search_buffer = "mutated_".to_string();
        a.last_search_pattern = Some("mutated_".to_string());
        a.search_next_in_diff();
    });

    app.cursor_to_top();
    app.set_diff_wrap(true);
    time_frames("scroll by one (wrap)", app, 100, |a| a.cursor_down(1));
    app.set_diff_wrap(false);

    app.diff_view_mode = crate::app::DiffViewMode::SideBySide;
    app.cursor_to_top();
    time_frames("scroll by one (sbs)", app, 100, |a| a.cursor_down(1));
}

fn report(files: usize, hunks: usize, lines: usize) {
    let total_lines = files * hunks * lines;
    println!(
        "\n=== fixture: {files} files x {hunks} hunks x {lines} lines = {total_lines} diff lines, viewport {WIDTH}x{HEIGHT} ==="
    );

    let t = Instant::now();
    let fixture = build_fixture(files, hunks, lines);
    println!("{:<28} {:>9.3?}", "fixture build (highlight)", t.elapsed());

    let t = Instant::now();
    let mut app = build_app(fixture);
    println!("{:<28} {:>9.3?}", "App::build", t.elapsed());

    // Initial frame, measured cold on its own terminal.
    {
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let t = Instant::now();
        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");
        println!("{:<28} {:>9.3?}", "initial frame", t.elapsed());
    }

    frame_battery(&mut app);
}

#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_large_diff() {
    report(300, 6, 30);
}

#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_small_diff() {
    report(10, 3, 20);
}

/// Break the load-time highlight cost into its phases so we can see which
/// part of `highlight_file_lines` dominates.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_highlight_breakdown() {
    let t = Instant::now();
    let highlighter = SyntaxHighlighter::default();
    println!(
        "\n{:<34} {:>9.3?}",
        "SyntaxHighlighter::default()",
        t.elapsed()
    );

    let path = Path::new("src/module_0/component_0.rs");
    let mut rng = Rng(0x2545F491_4F6CDD1D);
    let lines: Vec<String> = (0..30).map(|i| source_line(&mut rng, i)).collect();

    // syntax lookup alone
    let t = Instant::now();
    for _ in 0..3600 {
        std::hint::black_box(highlighter.syntax_set.find_syntax_for_file(path).unwrap());
    }
    println!(
        "{:<34} {:>9.3?}  (3600 calls)",
        "find_syntax_for_file",
        t.elapsed()
    );

    // HighlightLines::new alone
    let syntax = highlighter
        .syntax_set
        .find_syntax_for_file(path)
        .unwrap()
        .expect("syntax");
    let t = Instant::now();
    for _ in 0..3600 {
        std::hint::black_box(syntect::easy::HighlightLines::new(
            syntax,
            &highlighter.theme,
        ));
    }
    println!(
        "{:<34} {:>9.3?}  (3600 calls)",
        "HighlightLines::new",
        t.elapsed()
    );

    // Full highlight_file_lines on a 30-line hunk, 3600 times (300 files x 6 hunks x 2 sides)
    let t = Instant::now();
    for _ in 0..3600 {
        std::hint::black_box(highlighter.highlight_file_lines(path, &lines));
    }
    println!(
        "{:<34} {:>9.3?}  (3600 x 30 lines)",
        "highlight_file_lines",
        t.elapsed()
    );
}

/// Per-interaction state work that happens outside the render call.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_state_ops() {
    let fixture = build_fixture(300, 6, 30);
    let mut app = build_app(fixture);
    println!(
        "\n=== state ops: {} annotations ===",
        app.line_annotations.len()
    );

    let mut samples = Vec::new();
    for _ in 0..100 {
        let t = Instant::now();
        app.rebuild_annotations();
        samples.push(t.elapsed());
    }
    let s = stats(samples);
    println!(
        "{:<28} median {:>9.3?}  p95 {:>9.3?}",
        "rebuild_annotations", s.median, s.p95
    );
}

/// End-to-end load of a real repository's working-tree diff.
/// Point `TUICR_BENCH_REPO` at a checkout with a large uncommitted diff.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_real_repo_load() {
    let Ok(repo) = std::env::var("TUICR_BENCH_REPO") else {
        println!("\nTUICR_BENCH_REPO unset; skipping real-repo load bench");
        return;
    };
    std::env::set_current_dir(&repo).expect("chdir to bench repo");

    let backend = crate::vcs::git::GitBackend::discover(
        crate::vcs::git::GitBackendPreference::Libgit2,
        crate::vcs::DiffWhitespaceMode::Normal,
    )
    .expect("discover git repo");

    let highlighter = SyntaxHighlighter::default();

    let t = Instant::now();
    let files = backend
        .get_working_tree_diff(&highlighter)
        .expect("working tree diff");
    let load = t.elapsed();

    let total_lines: usize = files
        .iter()
        .flat_map(|f| f.hunks.iter())
        .map(|h| h.lines.len())
        .sum();
    println!(
        "\n=== real repo: {} files, {total_lines} diff lines ===",
        files.len()
    );
    println!("{:<28} {:>9.3?}", "get_working_tree_diff", load);

    let t = Instant::now();
    let mut app = build_app(files);
    println!("{:<28} {:>9.3?}", "App::build", t.elapsed());

    {
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let t = Instant::now();
        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");
        println!("{:<28} {:>9.3?}", "initial frame", t.elapsed());
    }

    frame_battery(&mut app);
}

/// Lazy highlighting must not change what lands on screen: render the real
/// repo's diff with lazy highlighting, then again with every file highlighted
/// up front, and compare the frame buffers cell for cell.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn lazy_highlight_renders_identically_to_eager() {
    let Ok(repo) = std::env::var("TUICR_BENCH_REPO") else {
        println!("\nTUICR_BENCH_REPO unset; skipping render equivalence check");
        return;
    };
    std::env::set_current_dir(&repo).expect("chdir to bench repo");

    let backend = crate::vcs::git::GitBackend::discover(
        crate::vcs::git::GitBackendPreference::Libgit2,
        crate::vcs::DiffWhitespaceMode::Normal,
    )
    .expect("discover git repo");
    let highlighter = SyntaxHighlighter::default();
    let files = backend
        .get_working_tree_diff(&highlighter)
        .expect("working tree diff");

    let mut lazy = build_app(files.clone());

    let mut eager_files = files;
    for file in &mut eager_files {
        let path = file.new_path.clone().or_else(|| file.old_path.clone());
        for hunk in &mut file.hunks {
            if let Some(path) = path.as_deref() {
                highlighter.highlight_hunk(path, hunk);
            }
            hunk.needs_highlight = false;
        }
    }
    let mut eager = build_app(eager_files);

    let mut compared = 0;
    for step in 0..40 {
        let draw = |app: &mut App| {
            let backend = TestBackend::new(WIDTH, HEIGHT);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
            terminal.backend().buffer().clone()
        };
        assert_eq!(
            draw(&mut lazy),
            draw(&mut eager),
            "frame {step} diverged between lazy and eager highlighting"
        );
        compared += 1;
        lazy.cursor_down(HEIGHT as usize / 2);
        eager.cursor_down(HEIGHT as usize / 2);
    }
    println!("\n{compared} frames identical between lazy and eager highlighting");
}

/// Synthetic unified diff shaped like `gh pr diff` output, so the PR open
/// path can be timed without a network round trip.
fn build_patch(files: usize, hunks_per_file: usize, lines_per_hunk: usize) -> String {
    let mut rng = Rng(0x2545F491_4F6CDD1D);
    let mut out = String::new();
    for f in 0..files {
        let path = format!("src/module_{}/component_{f}.rs", f % 17);
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        out.push_str("index 1111111..2222222 100644\n");
        out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
        for h in 0..hunks_per_file {
            let start = h * lines_per_hunk + 1;
            out.push_str(&format!(
                "@@ -{start},{lines_per_hunk} +{start},{lines_per_hunk} @@\n"
            ));
            for l in 0..lines_per_hunk {
                let prefix = match rng.below(4) {
                    0 => '+',
                    1 => '-',
                    _ => ' ',
                };
                out.push(prefix);
                out.push_str(&source_line(&mut rng, f * 1000 + h * 100 + l));
                out.push('\n');
            }
        }
    }
    out
}

/// Time the CPU-only half of the PR open (parse + `.tuicrignore` +
/// session build) so we can tell whether it belongs on the background
/// thread beside the network fetch.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_pr_open_prepare() {
    use crate::forge::traits::{ForgeRepository, PullRequestDetails};

    for (files, hunks, lines) in [(10usize, 3usize, 20usize), (300, 6, 30)] {
        let patch = build_patch(files, hunks, lines);
        println!(
            "\n=== patch: {files} files x {hunks} hunks x {lines} lines = {} KiB ===",
            patch.len() / 1024
        );

        let details = PullRequestDetails {
            repository: ForgeRepository::github("github.com", "agavra", "tuicr"),
            number: 1,
            title: String::new(),
            url: String::new(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: None,
            head_ref_name: "head".to_string(),
            base_ref_name: "main".to_string(),
            head_sha: "abcdef0123456789".to_string(),
            base_sha: "1234567890abcdef".to_string(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        };

        let t = Instant::now();
        let parsed = crate::vcs::diff_parser::parse_unified_diff(
            &patch,
            crate::vcs::diff_parser::DiffFormat::GitStyle,
        )
        .expect("parse");
        println!("{:<28} {:>9.3?}", "parse_unified_diff", t.elapsed());
        assert_eq!(parsed.len(), files);

        let t = Instant::now();
        let opened = crate::forge::pr_open::prepare_open_pr(
            details,
            &patch,
            Vec::new(),
            Default::default(),
            None,
        )
        .expect("prepare");
        println!("{:<28} {:>9.3?}", "prepare_open_pr (total)", t.elapsed());
        assert_eq!(opened.diff_files.len(), files);
    }
}

/// Worst case for per-file laziness: one very large file, whose whole
/// highlight cost lands on the frame that first shows it.
#[test]
#[ignore = "timing harness; run explicitly with --ignored"]
fn perf_bench_single_huge_file() {
    let mut files = build_fixture(2, 200, 100);
    for file in &mut files {
        for hunk in &mut file.hunks {
            for line in &mut hunk.lines {
                line.highlighted_spans = None;
            }
        }
    }
    let lines: usize = files[0].hunks.iter().map(|h| h.lines.len()).sum();
    println!("\n=== single file of {lines} diff lines ===");

    let mut app = build_app(files);
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal");

    let t = Instant::now();
    terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .expect("draw");
    println!("{:<28} {:>9.3?}", "first frame on file 1", t.elapsed());

    app.next_file();
    let t = Instant::now();
    terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .expect("draw");
    println!("{:<28} {:>9.3?}", "first frame on file 2", t.elapsed());
}
