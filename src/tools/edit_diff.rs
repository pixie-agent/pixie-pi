//! Exact-replace edit application + unified-diff generation
//! (`core/tools/edit-diff.ts`).

use serde::{Deserialize, Serialize};

/// One targeted replacement. `old_text` must match a unique, non-overlapping
/// region of the original file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    #[serde(rename = "oldText")]
    pub old_text: String,
    #[serde(rename = "newText")]
    pub new_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

/// Strip a UTF-8 BOM if present. Returns (bom, text).
pub fn strip_bom(raw: &str) -> (String, String) {
    if let Some(rest) = raw.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), rest.to_string())
    } else {
        (String::new(), raw.to_string())
    }
}

pub fn detect_line_ending(s: &str) -> LineEnding {
    if s.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Normalize all line endings to LF.
pub fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Restore the original line ending style.
pub fn restore_line_endings(s: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => s.to_string(),
        LineEnding::Crlf => s.replace('\n', "\r\n"),
    }
}

/// Normalize text for fuzzy matching, mirroring pi's `normalizeForFuzzyMatch`:
/// NFKC-fold, strip per-line trailing whitespace, and fold smart quotes / dashes
/// / special spaces to their ASCII equivalents. Lets an edit land when the
/// model's `oldText` differs only in cosmetic whitespace or Unicode look-alikes.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    const SMART_APOSTROPHES: &[char] = &['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];
    const SMART_QUOTES: &[char] = &['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'];
    const DASHES: &[char] = &['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}'];
    const SPECIAL_SPACES: &[char] = &[
        '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}',
        '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
    ];
    let nfkc: String = text.nfkc().collect();
    let trimmed = nfkc
        .split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    trimmed
        .replace(SMART_APOSTROPHES, "'")
        .replace(SMART_QUOTES, "\"")
        .replace(DASHES, "-")
        .replace(SPECIAL_SPACES, " ")
}

/// True when `old` is absent from `content` exactly but present after
/// fuzzy-normalizing both — i.e. a fuzzy fallback would rescue this edit.
fn fuzzy_needed(content: &str, old: &str) -> bool {
    if content.find(old).is_some() {
        return false;
    }
    let fo = normalize_for_fuzzy_match(old);
    !fo.is_empty() && normalize_for_fuzzy_match(content).find(&fo).is_some()
}

/// Find `old` in `base`: exact match first (byte index + byte len), else a
/// match in fuzzy-normalized space. Returns `(start, length)` in `base`'s
/// coordinates. Mirrors pi's `fuzzyFindText`.
fn find_in_base(base: &str, old: &str) -> Option<(usize, usize)> {
    if let Some(i) = base.find(old) {
        return Some((i, old.len()));
    }
    let fo = normalize_for_fuzzy_match(old);
    if fo.is_empty() {
        return None;
    }
    normalize_for_fuzzy_match(base)
        .find(fo.as_str())
        .map(|i| (i, fo.len()))
}

/// Occurrence count in fuzzy space. Mirrors pi's `countOccurrences`, which
/// always normalizes — so two regions differing only in trailing whitespace (or
/// Unicode look-alikes) count as duplicates and are rejected as ambiguous.
fn count_occurrences(content: &str, old: &str) -> usize {
    let fo = normalize_for_fuzzy_match(old);
    if fo.is_empty() {
        return 0;
    }
    normalize_for_fuzzy_match(content).matches(fo.as_str()).count()
}

/// Splice non-overlapping `(start, len, new_text)` spans into `base` in order.
fn splice_replacements(base: &str, spans: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(base.len());
    let mut cursor = 0;
    for (start, len, new_text) in spans {
        out.push_str(&base[cursor..*start]);
        out.push_str(new_text);
        cursor = *start + *len;
    }
    out.push_str(&base[cursor..]);
    out
}

/// Split `content` into lines, each keeping its trailing '\n' (the final line is
/// included only if non-empty). Mirrors pi's `splitLinesWithEndings`.
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            out.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

/// Cumulative `(start_byte, end_byte)` span of each line (with its ending).
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in split_lines_with_endings(content) {
        let end = offset + line.len();
        spans.push((offset, end));
        offset = end;
    }
    spans
}

/// The half-open `[start_line, end_line)` range of lines a `(start, len)`
/// replacement touches, in `spans` coordinates. Mirrors `getReplacementLineRange`.
fn replacement_line_range(spans: &[(usize, usize)], start: usize, len: usize) -> Option<(usize, usize)> {
    let end = start + len;
    let start_line = spans.iter().position(|(s, e)| *s <= start && start < *e)?;
    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < end {
        end_line += 1;
    }
    Some((start_line, end_line + 1))
}

/// Apply `reps` (absolute base coords) within a sliced region of the base, in
/// reverse order so earlier offsets stay valid. `offset` is the slice's start in
/// base coords. Mirrors pi's `applyReplacements`.
fn apply_replacements_in_slice(slice: &str, reps: &[&(usize, usize, String)], offset: usize) -> String {
    let mut result = slice.to_string();
    for (at, len, new_text) in reps.iter().rev() {
        let rel_start = *at - offset;
        let rel_end = rel_start + *len;
        result = format!("{}{}{}", &result[..rel_start], new_text, &result[rel_end..]);
    }
    result
}

/// Overlay `replacements` (matched in the fuzzy-normalized `base`) onto the
/// `original` content: only the lines a replacement touches are rewritten from
/// the normalized base; every other line is copied byte-for-byte from the
/// original. This is what makes fuzzy matching safe — NFKC / trailing-whitespace
/// folding can never corrupt untouched lines. Mirrors pi's
/// `applyReplacementsPreservingUnchangedLines`.
fn apply_replacements_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[(usize, usize, String)],
) -> String {
    let original_lines = split_lines_with_endings(original);
    let base_spans = line_spans(base);
    // Fuzzy normalization only changes within-line bytes, never the line count,
    // so the two line views stay aligned one-to-one. (If they ever diverged,
    // overlaying would misalign — fall back to the original in that case.)
    if original_lines.len() != base_spans.len() {
        return original.to_string();
    }

    // Group replacements whose touched line-ranges overlap or abut.
    type RepRef<'a> = &'a (usize, usize, String);
    let mut sorted: Vec<RepRef<'_>> = replacements.iter().collect();
    sorted.sort_by_key(|r| r.0);
    let mut groups: Vec<(usize, usize, Vec<RepRef<'_>>)> = Vec::new();
    for rep in &sorted {
        let Some((sl, el)) = replacement_line_range(&base_spans, rep.0, rep.1) else {
            continue;
        };
        if let Some(last) = groups.last_mut() {
            if sl < last.1 {
                last.1 = last.1.max(el);
                last.2.push(*rep);
                continue;
            }
        }
        groups.push((sl, el, vec![*rep]));
    }

    let mut result = String::new();
    let mut original_line_idx = 0usize;
    for (start_line, end_line, group_reps) in &groups {
        result.extend(original_lines[original_line_idx..*start_line].iter().copied());
        let group_start_offset = base_spans[*start_line].0;
        let group_end_offset = base_spans[*end_line - 1].1;
        let slice = &base[group_start_offset..group_end_offset];
        result.push_str(&apply_replacements_in_slice(slice, group_reps, group_start_offset));
        original_line_idx = *end_line;
    }
    result.extend(original_lines[original_line_idx..].iter().copied());
    result
}

/// Apply a set of edits to LF-normalized content. Each `old_text` is matched
/// against the original (not incrementally); edits must be unique and
/// non-overlapping. As a fallback, when an exact match fails the search retries
/// in fuzzy-normalized space (mirrors pi's `applyEditsToNormalizedContent`). If
/// any edit needs fuzzy matching, all edits are applied in that space and then
/// overlaid onto the original line-by-line so untouched lines keep their bytes.
/// Returns `(base, new)` for diffing.
pub fn apply_edits(content: &str, edits: &[Edit], path: &str) -> anyhow::Result<(String, String)> {
    // Normalize each edit's search/replace text to LF: the file content is
    // already LF-normalized by the caller before matching, so a CRLF oldText
    // (copied by the model from a CRLF file) would never match, and a CRLF
    // newText would produce `\r\r\n` when line endings are restored.
    let normalized_edits: Vec<(String, String)> = edits
        .iter()
        .map(|e| (normalize_to_lf(&e.old_text), normalize_to_lf(&e.new_text)))
        .collect();
    for (old, _) in &normalized_edits {
        if old.is_empty() {
            anyhow::bail!("Could not edit file: {path}. An edits[].oldText is empty.");
        }
    }

    // If ANY edit fails the exact match but hits in fuzzy space, run the whole
    // edit set in fuzzy space — you can't mix offsets across the two spaces —
    // then overlay onto the original so unchanged lines keep their exact bytes.
    let used_fuzzy = normalized_edits.iter().any(|(old, _)| fuzzy_needed(content, old));
    let replacement_base: String = if used_fuzzy {
        normalize_for_fuzzy_match(content)
    } else {
        content.to_string()
    };

    // Locate each edit's byte span in the replacement base.
    let mut spans: Vec<(usize, usize, String)> = Vec::new(); // (start, len, new_text)
    for (old, new) in &normalized_edits {
        let Some((start, mlen)) = find_in_base(&replacement_base, old) else {
            anyhow::bail!(
                "Could not edit file: {path}. The oldText was not found. Make sure oldText matches the file exactly (including indentation and whitespace)."
            );
        };
        let occ = count_occurrences(&replacement_base, old);
        if occ > 1 {
            anyhow::bail!(
                "Could not edit file: {path}. The oldText is not unique (found {occ} matches). Include more surrounding context so oldText matches exactly one location."
            );
        }
        spans.push((start, mlen, new.clone()));
    }

    // Sort by start position and reject overlaps.
    spans.sort_by_key(|(s, _, _)| *s);
    for window in spans.windows(2) {
        let (a_start, a_len, _) = &window[0];
        let (b_start, _, _) = &window[1];
        if *b_start < *a_start + *a_len {
            anyhow::bail!(
                "Could not edit file: {path}. Overlapping or nested edits are not allowed. Merge nearby changes into a single edit."
            );
        }
    }

    let new_content = if used_fuzzy {
        apply_replacements_preserving_unchanged_lines(content, &replacement_base, &spans)
    } else {
        splice_replacements(&replacement_base, &spans)
    };

    Ok((content.to_string(), new_content))
}

/// Generate a unified diff between two strings (line-based, 3 lines of
/// context). Returns `(diff_string, first_changed_line)` where
/// `first_changed_line` is 1-indexed in the new file.
pub fn generate_diff(base: &str, new: &str, path: &str) -> (String, Option<usize>) {
    // Split on '\n' but drop the trailing empty element produced when the
    // content ends with a newline (otherwise the last hunk carries a phantom
    // empty context line and an off-by-one line count).
    let mut base_lines: Vec<&str> = base.split('\n').collect();
    if base.ends_with('\n') {
        base_lines.pop();
    }
    let mut new_lines: Vec<&str> = new.split('\n').collect();
    if new.ends_with('\n') {
        new_lines.pop();
    }

    let ops = diff_lines(&base_lines, &new_lines);
    if ops.iter().all(|o| matches!(o, DiffOp::Equal(_))) {
        return (String::new(), None);
    }

    let context = 3;
    let hunks = group_hunks(&ops, context);

    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ {path}\n"));
    let mut first_changed_line: Option<usize> = None;
    // Cumulative line counters across all preceding ops.
    let mut old_prefix = 0usize;
    let mut new_prefix = 0usize;
    let mut consumed = 0usize;

    for hunk in &hunks {
        // Advance counters over the ops before this hunk.
        for op in &ops[consumed..hunk.start] {
            match op {
                DiffOp::Equal(_) => {
                    old_prefix += 1;
                    new_prefix += 1;
                }
                DiffOp::Delete(_) => old_prefix += 1,
                DiffOp::Insert(_) => new_prefix += 1,
            }
        }
        consumed = hunk.end;

        let old_start = old_prefix + 1;
        let new_start = new_prefix + 1;
        let (mut old_count, mut new_count) = (0usize, 0usize);
        let mut new_cursor = new_prefix;
        let mut body = String::new();
        for op in &ops[hunk.start..hunk.end] {
            match op {
                DiffOp::Equal(l) => {
                    body.push_str(&format!(" {l}\n"));
                    old_count += 1;
                    new_count += 1;
                    new_cursor += 1;
                }
                DiffOp::Delete(l) => {
                    if first_changed_line.is_none() {
                        first_changed_line = Some(new_cursor + 1);
                    }
                    body.push_str(&format!("-{l}\n"));
                    old_count += 1;
                }
                DiffOp::Insert(l) => {
                    if first_changed_line.is_none() {
                        first_changed_line = Some(new_cursor + 1);
                    }
                    body.push_str(&format!("+{l}\n"));
                    new_count += 1;
                    new_cursor += 1;
                }
            }
        }
        // Advance the running counters over this hunk's body so the next hunk
        // computes its `@@ -start +start @@` line numbers relative to the true
        // position in the file. (Without this, only inter-hunk gaps were counted
        // and every hunk after the first reported stale, too-small line numbers.)
        old_prefix += old_count;
        new_prefix += new_count;
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n{}",
            old_start,
            old_count.max(1),
            new_start,
            new_count.max(1),
            body
        ));
    }
    (out, first_changed_line)
}

#[derive(Debug, Clone, Copy)]
enum DiffOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

fn is_change(op: &DiffOp) -> bool {
    matches!(op, DiffOp::Delete(_) | DiffOp::Insert(_))
}

struct HunkRange {
    start: usize,
    end: usize,
}

/// Partition `ops` into maximal runs of ops that lie within `context` ops of a
/// change. Returns half-open `[start, end)` ranges.
fn group_hunks(ops: &[DiffOp], context: usize) -> Vec<HunkRange> {
    let n = ops.len();
    let in_hunk: Vec<bool> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(n);
            ops[lo..hi].iter().any(is_change)
        })
        .collect();

    let mut hunks = Vec::new();
    let mut i = 0;
    while i < n {
        if in_hunk[i] {
            let start = i;
            while i < n && in_hunk[i] {
                i += 1;
            }
            hunks.push(HunkRange { start, end: i });
        } else {
            i += 1;
        }
    }
    hunks
}

/// Classic LCS line diff.
///
/// The LCS DP is O(n·m) in time *and* memory, and `generate_diff` runs it on the
/// **whole** file after every edit. To keep an edit to a large file from
/// allocating a multi-gigabyte table, we first strip the common leading and
/// trailing lines — which a full LCS would only ever match as `Equal` anyway, so
/// the resulting op sequence is identical — and run the DP over just the changed
/// middle. For the pathological case where that middle is itself enormous (a
/// full rewrite of a huge file), a cell-budget guard skips the DP and emits a
/// coarse delete/insert hunk; the diff is cosmetic model feedback, so that is
/// preferable to hanging or OOMing the agent mid-edit.
fn diff_lines<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    // Common leading lines.
    let mut prefix = 0;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    // Common trailing lines, kept from overlapping the prefix.
    let mut suffix = 0;
    while prefix + suffix < a.len()
        && prefix + suffix < b.len()
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];

    let mut ops = Vec::with_capacity(a.len() + b.len());
    for l in &a[..prefix] {
        ops.push(DiffOp::Equal(l));
    }
    ops.extend(diff_middle(mid_a, mid_b));
    for l in &a[a.len() - suffix..] {
        ops.push(DiffOp::Equal(l));
    }
    ops
}

/// LCS the changed middle, with a cell-budget guard against pathological inputs.
fn diff_middle<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    /// Max LCS-table cells we are willing to allocate (~16 MiB of `usize`).
    const DP_CELL_BUDGET: usize = 2_000_000;
    let n = a.len();
    let m = b.len();

    // Over budget (or the product would overflow): skip the DP and emit a coarse
    // delete-old / insert-new diff. Memory is bounded to the ops vec — no OOM.
    if n.checked_mul(m).is_none_or(|c| c > DP_CELL_BUDGET) {
        let mut ops = Vec::with_capacity(n + m);
        for l in a {
            ops.push(DiffOp::Delete(l));
        }
        for l in b {
            ops.push(DiffOp::Insert(l));
        }
        return ops;
    }

    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal(a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(DiffOp::Delete(a[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Insert(b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Delete(a[i]));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Insert(b[j]));
        j += 1;
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_unique_non_overlapping_edits() {
        let content = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let edits = vec![
            Edit {
                old_text: "let x = 1;".into(),
                new_text: "let x = 42;".into(),
            },
            Edit {
                old_text: "let y = 2;".into(),
                new_text: "let y = 99;".into(),
            },
        ];
        let (base, new) = apply_edits(content, &edits, "f.rs").unwrap();
        assert!(new.contains("let x = 42;"));
        assert!(new.contains("let y = 99;"));
        assert!(!base.contains("let x = 42;"));
    }

    #[test]
    fn rejects_non_unique_oldtext() {
        let content = "a\na\n";
        let edits = vec![Edit {
            old_text: "a".into(),
            new_text: "b".into(),
        }];
        let err = apply_edits(content, &edits, "f").unwrap_err();
        assert!(format!("{err}").contains("not unique"));
    }

    #[test]
    fn rejects_overlapping_edits() {
        let content = "abcdef";
        let edits = vec![
            Edit {
                old_text: "abcd".into(),
                new_text: "X".into(),
            },
            Edit {
                old_text: "cdef".into(),
                new_text: "Y".into(),
            },
        ];
        let err = apply_edits(content, &edits, "f").unwrap_err();
        assert!(format!("{err}").contains("Overlap"));
    }

    #[test]
    fn fuzzy_rescues_trailing_whitespace_and_preserves_untouched_lines() {
        // The file's `foo  ` line has trailing spaces the model omitted from
        // oldText ("foo\nbar"), so the exact match fails. Fuzzy (per-line
        // trim_end) rescues it. Crucially, only the touched lines are rewritten
        // from the normalized base; the untouched `header` / `footer` lines must
        // keep their original trailing whitespace byte-for-byte.
        let content = "header   \nfoo  \nbar\nfooter   \n";
        let edits = vec![Edit {
            old_text: "foo\nbar".into(),
            new_text: "REPLACED".into(),
        }];
        let (base, new) = apply_edits(content, &edits, "f.rs").unwrap();
        // Only the touched `foo  \nbar` region is rewritten; the untouched
        // `header` and `footer` lines keep their trailing whitespace verbatim.
        assert_eq!(
            new, "header   \nREPLACED\nfooter   \n",
            "fuzzy edit must replace only the touched lines: {new:?}"
        );
        assert_eq!(base, content, "base is the original content");
    }

    #[test]
    fn fuzzy_rescues_smart_quotes_and_dashes() {
        // File uses a smart apostrophe (') and em dash (—); oldText uses straight
        // quotes / hyphen. Fuzzy normalization folds both, so the edit lands.
        let content = "it\u{2019}s here\u{2014}now\n";
        let edits = vec![Edit {
            old_text: "it's here-now".into(),
            new_text: "done".into(),
        }];
        let (_base, new) = apply_edits(content, &edits, "f").unwrap();
        assert!(new.contains("done"), "{new}");
    }

    #[test]
    fn fuzzy_count_rejects_whitespace_only_duplicate() {
        // oldText "foo" matches `foo   ` exactly, but the file has a second
        // `foo` line — count_occurrences normalizes, so it sees 2 and rejects as
        // ambiguous rather than silently editing the first.
        let content = "foo   \nfoo\n";
        let edits = vec![Edit {
            old_text: "foo".into(),
            new_text: "bar".into(),
        }];
        let err = apply_edits(content, &edits, "f").unwrap_err();
        assert!(format!("{err}").contains("not unique"), "{err}");
        assert!(format!("{err}").contains("2 matches"), "{err}");
    }

    #[test]
    fn fuzzy_genuinely_absent_oldtext_still_errors() {
        // Not in the file even after normalization → must error, not silently no-op.
        let content = "hello world\n";
        let edits = vec![Edit {
            old_text: "missing".into(),
            new_text: "x".into(),
        }];
        let err = apply_edits(content, &edits, "f").unwrap_err();
        assert!(format!("{err}").contains("was not found"), "{err}");
    }

    #[test]
    fn diff_shows_changes() {
        let (diff, first) = generate_diff("a\nb\nc\n", "a\nB\nc\n", "f");
        assert!(diff.contains("-b"));
        assert!(diff.contains("+B"));
        assert_eq!(first, Some(2));
    }

    #[test]
    fn matches_crlf_oldtext() {
        // The file content is LF-normalized by the caller before matching; the
        // search text is normalized too, so a model that echoes CRLF still hits.
        let normalized = normalize_to_lf("a\r\nb\r\n");
        let edits = vec![Edit {
            old_text: "a\r\nb".into(),
            new_text: "c\nd".into(),
        }];
        let (_, new) = apply_edits(&normalized, &edits, "f").unwrap();
        assert!(new.contains("c\nd"));
    }

    #[test]
    fn crlf_restore_has_no_doubled_carriage_returns() {
        let normalized = normalize_to_lf("fn main() {\r\n    let x = 1;\r\n}\r\n");
        let edits = vec![Edit {
            old_text: "let x = 1;".into(),
            new_text: "let x = 2;\n    let y = 3;".into(),
        }];
        let (_, new_normalized) = apply_edits(&normalized, &edits, "f.rs").unwrap();
        let restored = restore_line_endings(&new_normalized, LineEnding::Crlf);
        assert!(!restored.contains("\r\r"), "doubled CR after restore");
        assert_eq!(
            restored,
            "fn main() {\r\n    let x = 2;\r\n    let y = 3;\r\n}\r\n"
        );
    }

    #[test]
    fn multihunk_diff_has_correct_line_numbers() {
        // Two single-line changes 10 lines apart produce two hunks. The second
        // hunk's change is at line 11; with 3 lines of context it must start at
        // line 8 in both old and new files. (Previously the 2nd hunk header read
        // `-4,5` because the running line counters were never advanced over a
        // hunk's own body — only over the gap before it.)
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n";
        let new = "X\n2\n3\n4\n5\n6\n7\n8\n9\n10\nY\n";
        let (diff, first) = generate_diff(base, new, "f");
        assert!(
            diff.contains("@@ -8,4 +8,4 @@"),
            "second hunk header should be @@ -8,4 +8,4 @@\ngot:\n{diff}"
        );
        assert_eq!(first, Some(1));
        // No phantom empty trailing context line from the final newline.
        assert!(
            !diff.ends_with(" \n"),
            "diff should not end with a spurious empty context line, got:\n{diff:?}"
        );
    }

    #[test]
    fn small_edit_in_a_large_file_produces_a_focused_diff() {
        // One line changed in a 2000-line file. The common prefix/suffix must be
        // stripped before the LCS so the DP runs over a 1×1 middle, not 2000×2000
        // (4M cells). The diff is the same a full LCS would emit: one focused
        // hunk around the change. The far-away unchanged lines must NOT appear.
        let base: String = (0..2000)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = {
            let mut lines: Vec<String> = (0..2000).map(|i| format!("line-{i}")).collect();
            lines[1234] = "line-1234-CHANGED".into();
            lines.join("\n") + "\n"
        };
        let (diff, first) = generate_diff(&base, &new, "f");
        assert_eq!(first, Some(1235), "first changed line is line 1235 (1-indexed)");
        assert!(diff.contains("-line-1234\n"), "{diff}");
        assert!(diff.contains("+line-1234-CHANGED\n"), "{diff}");
        assert!(
            diff.contains("@@ -1232,7 +1232,7 @@\n"),
            "expected a single focused hunk header, got:\n{diff}"
        );
        // Unchanged lines far from the edit must be absent.
        assert!(!diff.contains("line-0\n"));
        assert!(!diff.contains("line-1999\n"));
    }

    #[test]
    fn large_full_rewrite_uses_the_bounded_fallback() {
        // Every line differs, so the changed middle is 2000×2000 = 4M cells —
        // over the DP budget. The LCS table must NOT be built; the fallback emits
        // a coarse delete-old / insert-new hunk and returns bounded output
        // without allocating (and without OOMing on a genuinely huge rewrite).
        let base: String = (0..2000).map(|i| format!("old-{i}\n")).collect();
        let new: String = (0..2000).map(|i| format!("new-{i}\n")).collect();
        let (diff, first) = generate_diff(&base, &new, "big.txt");
        assert!(diff.starts_with("--- big.txt\n+++ big.txt\n"), "{diff}");
        assert_eq!(first, Some(1));
        // Coarse fallback: all old lines deleted, all new lines inserted.
        assert!(diff.contains("-old-0\n"), "{diff}");
        assert!(diff.contains("+new-0\n"), "{diff}");
    }
}
