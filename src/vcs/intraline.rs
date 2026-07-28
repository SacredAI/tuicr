//! Word-level differences inside a changed line.
//!
//! A unified diff says which lines changed but not which words. When a hunk
//! replaces a run of lines, this pass pairs each removed line with the added
//! line in the same position and records the byte ranges that differ, so the
//! renderers can tint the changed words more strongly than the line.
//!
//! Runs once per diff load, not per frame.

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::model::{DiffHunk, DiffLine, LineOrigin};

/// Fraction of a line's bytes that must survive unchanged for the word ranges
/// to be kept. A rewritten line pairs with something unrelated, and marking
/// every word reads as noise, so below this the line keeps its plain tint.
const MIN_SHARED_FRACTION: f64 = 0.5;

/// Lines longer than this skip the word diff. Minified assets and generated
/// data pair into thousands of tokens for no readable gain.
const MAX_LINE_BYTES: usize = 4096;

/// Record word-level ranges on every removed/added line pair in `hunks`.
pub(crate) fn annotate_hunks(hunks: &mut [DiffHunk]) {
    for hunk in hunks {
        annotate_lines(&mut hunk.lines);
    }
}

fn annotate_lines(lines: &mut [DiffLine]) {
    let mut idx = 0;
    while idx < lines.len() {
        let del_start = idx;
        while idx < lines.len() && lines[idx].origin == LineOrigin::Deletion {
            idx += 1;
        }
        let add_start = idx;
        while idx < lines.len() && lines[idx].origin == LineOrigin::Addition {
            idx += 1;
        }

        if del_start == add_start || add_start == idx {
            // Not a removed run immediately followed by an added run.
            idx = (del_start + 1).max(idx);
            continue;
        }

        let pairs = (add_start - del_start).min(idx - add_start);
        for offset in 0..pairs {
            let del_idx = del_start + offset;
            let add_idx = add_start + offset;
            let Some((old_ranges, new_ranges)) =
                changed_ranges(&lines[del_idx].content, &lines[add_idx].content)
            else {
                continue;
            };
            lines[del_idx].intraline = old_ranges;
            lines[add_idx].intraline = new_ranges;
        }
    }
}

/// Changed byte ranges on the old side and the new side of a paired line.
type ChangedRanges = (Vec<Range<usize>>, Vec<Range<usize>>);

/// Byte ranges of the words that differ between `old` and `new`, or `None`
/// when the two lines are too dissimilar to align word by word.
pub(crate) fn changed_ranges(old: &str, new: &str) -> Option<ChangedRanges> {
    if old == new || old.is_empty() || new.is_empty() {
        return None;
    }
    if old.len() > MAX_LINE_BYTES || new.len() > MAX_LINE_BYTES {
        return None;
    }

    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let old_words: Vec<&str> = old_tokens.iter().map(|r| &old[r.clone()]).collect();
    let new_words: Vec<&str> = new_tokens.iter().map(|r| &new[r.clone()]).collect();

    let mut old_changed = Vec::new();
    let mut new_changed = Vec::new();
    let mut shared = 0usize;

    for op in capture_diff_slices(Algorithm::Patience, &old_words, &new_words) {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                shared += old_tokens[old_index..old_index + len]
                    .iter()
                    .map(|r| r.len())
                    .sum::<usize>();
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => push_span(
                &mut old_changed,
                &old_tokens[old_index..old_index + old_len],
            ),
            DiffOp::Insert {
                new_index, new_len, ..
            } => push_span(
                &mut new_changed,
                &new_tokens[new_index..new_index + new_len],
            ),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                push_span(
                    &mut old_changed,
                    &old_tokens[old_index..old_index + old_len],
                );
                push_span(
                    &mut new_changed,
                    &new_tokens[new_index..new_index + new_len],
                );
            }
        }
    }

    if old_changed.is_empty() && new_changed.is_empty() {
        return None;
    }
    if (shared as f64) < MIN_SHARED_FRACTION * old.len().max(new.len()) as f64 {
        return None;
    }

    Some((old_changed, new_changed))
}

/// Append the span covering `tokens`, merging it into the previous span when
/// they touch, so adjacent changed tokens paint as one block.
fn push_span(out: &mut Vec<Range<usize>>, tokens: &[Range<usize>]) {
    let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else {
        return;
    };
    let span = first.start..last.end;
    match out.last_mut() {
        Some(prev) if prev.end >= span.start => prev.end = span.end,
        _ => out.push(span),
    }
}

/// Split a line into identifier runs, whitespace runs, and single punctuation
/// characters. Splitting punctuation individually keeps `a.b()` from aligning
/// as one token, which is what makes the highlight land on the changed name
/// rather than the whole expression.
fn tokenize(line: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        let class = TokenClass::of(ch);
        let mut end = start + ch.len_utf8();
        if class != TokenClass::Punctuation {
            while let Some(&(next_start, next_ch)) = chars.peek() {
                if TokenClass::of(next_ch) != class {
                    break;
                }
                end = next_start + next_ch.len_utf8();
                chars.next();
            }
        }
        tokens.push(start..end);
    }

    tokens
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum TokenClass {
    Word,
    Whitespace,
    Punctuation,
}

impl TokenClass {
    fn of(ch: char) -> Self {
        if ch.is_whitespace() {
            TokenClass::Whitespace
        } else if ch.is_alphanumeric() || ch == '_' {
            TokenClass::Word
        } else {
            TokenClass::Punctuation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(origin: LineOrigin, content: &str) -> DiffLine {
        DiffLine {
            origin,
            content: content.to_string(),
            old_lineno: None,
            new_lineno: None,
            highlighted_spans: None,
            intraline: Vec::new(),
        }
    }

    fn spans<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|r| &text[r.clone()]).collect()
    }

    #[test]
    fn should_mark_only_the_renamed_identifier() {
        let old = "let total = compute_sum(values);";
        let new = "let total = compute_mean(values);";
        let (old_ranges, new_ranges) = changed_ranges(old, new).expect("lines should align");

        assert_eq!(spans(old, &old_ranges), vec!["compute_sum"]);
        assert_eq!(spans(new, &new_ranges), vec!["compute_mean"]);
    }

    #[test]
    fn should_merge_adjacent_changed_tokens_into_one_span() {
        let old = "foo(a, b)";
        let new = "foo(a.b, b)";
        let (_, new_ranges) = changed_ranges(old, new).expect("lines should align");

        assert_eq!(spans(new, &new_ranges), vec![".b"]);
    }

    #[test]
    fn should_report_insertion_on_the_new_side_only() {
        let old = "fn run(x: u32) {";
        let new = "fn run(x: u32, y: u32) {";
        let (old_ranges, new_ranges) = changed_ranges(old, new).expect("lines should align");

        assert!(old_ranges.is_empty());
        assert_eq!(spans(new, &new_ranges), vec![", y: u32"]);
    }

    #[test]
    fn should_skip_pairs_that_share_too_little_text() {
        assert!(changed_ranges("let total = compute_sum(values);", "return Ok(());").is_none());
    }

    #[test]
    fn should_pair_removed_and_added_lines_by_position() {
        let mut lines = vec![
            line(LineOrigin::Context, "fn main() {"),
            line(LineOrigin::Deletion, "    let a = 1;"),
            line(LineOrigin::Deletion, "    let b = 2;"),
            line(LineOrigin::Addition, "    let a = 3;"),
            line(LineOrigin::Addition, "    let b = 4;"),
            line(LineOrigin::Context, "}"),
        ];
        annotate_lines(&mut lines);

        assert!(lines[0].intraline.is_empty());
        assert_eq!(spans(&lines[1].content, &lines[1].intraline), vec!["1"]);
        assert_eq!(spans(&lines[2].content, &lines[2].intraline), vec!["2"]);
        assert_eq!(spans(&lines[3].content, &lines[3].intraline), vec!["3"]);
        assert_eq!(spans(&lines[4].content, &lines[4].intraline), vec!["4"]);
        assert!(lines[5].intraline.is_empty());
    }

    #[test]
    fn should_leave_a_pure_addition_run_unmarked() {
        let mut lines = vec![
            line(LineOrigin::Context, "fn main() {"),
            line(LineOrigin::Addition, "    let a = 1;"),
            line(LineOrigin::Context, "}"),
        ];
        annotate_lines(&mut lines);

        assert!(lines.iter().all(|l| l.intraline.is_empty()));
    }

    #[test]
    fn should_mark_ranges_that_slice_multibyte_content_cleanly() {
        let old = "let label = \"café au lait\";";
        let new = "let label = \"café au thé\";";
        let (old_ranges, new_ranges) = changed_ranges(old, new).expect("lines should align");

        assert_eq!(spans(old, &old_ranges), vec!["lait"]);
        assert_eq!(spans(new, &new_ranges), vec!["thé"]);
    }
}
