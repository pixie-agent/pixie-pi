//! Shared truncation utilities (`core/tools/truncate.ts`).
//!
//! Tools return bounded output: `read`/`write`/`ls` use head-truncation,
//! `bash` uses tail-truncation (keep the most recent output), and `grep`
//! truncates long individual lines.

use serde::{Deserialize, Serialize};

/// Default line cap for tool output (matches Claude Code / pi).
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default byte cap for tool output (50 KiB). Matches pi's `core/tools/truncate.ts`.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Maximum length of a single grep result line. Matches pi's `GREP_MAX_LINE_LENGTH`.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<String>, // "lines" | "bytes"
    pub output_lines: usize,
    pub total_lines: usize,
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
    pub first_line_exceeds_limit: bool,
    pub output_bytes: usize,
    pub last_line_partial: bool,
}

/// Truncate from the **head** (keep the first lines), used by read/write/ls.
/// Stops at whichever limit is hit first: `max_lines` or `max_bytes`.
pub fn truncate_head(content: &str, max_lines: Option<usize>, max_bytes: Option<usize>) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let mut lines: Vec<&str> = content.split('\n').collect();
    // A trailing '\n' splits into a phantom empty final element (nearly every
    // command output and source file ends in a newline). Drop it so a 3-line
    // input counts as 3 lines, not 4 — otherwise `total_lines` is inflated and
    // the line cap reports a spurious truncation or drops a real line. Mirrors
    // the same accounting already applied in `read.rs` and `generate_diff`.
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();

    if !lines.is_empty() {
        let first_line_bytes = lines[0].len();
        if first_line_bytes > max_bytes {
            return TruncationResult {
                first_line_exceeds_limit: true,
                total_lines,
                max_bytes: Some(max_bytes),
                max_lines: Some(max_lines),
                ..Default::default()
            };
        }
    }

    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by: Option<String> = None;

    for line in &lines {
        let line_bytes = line.len() + 1; // +1 for the '\n' separator
        if out.len() + 1 > max_lines {
            truncated_by = Some("lines".into());
            break;
        }
        if bytes + line_bytes > max_bytes {
            truncated_by = Some("bytes".into());
            break;
        }
        bytes += line_bytes;
        out.push(line);
    }

    let truncated = truncated_by.is_some();
    let content = out.join("\n");
    TruncationResult {
        output_bytes: content.len(),
        content,
        truncated,
        truncated_by,
        output_lines: out.len(),
        total_lines,
        max_lines: Some(max_lines),
        max_bytes: Some(max_bytes),
        first_line_exceeds_limit: false,
        last_line_partial: false,
    }
}

/// Truncate from the **tail** (keep the most recent output), used by bash.
pub fn truncate_tail(content: &str, max_lines: Option<usize>, max_bytes: Option<usize>) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let mut lines: Vec<&str> = content.split('\n').collect();
    // Drop the phantom empty element produced by a trailing newline so the
    // reported line count is exact. Bash passes raw command output (which almost
    // always ends in '\n') here and reads `total_lines` / `output_lines` for its
    // "Showing lines X-Y of N" truncation notice — without this drop that notice
    // is off by one. See `truncate_head` for the full rationale.
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();

    // Keep the last `max_lines` lines.
    let start = total_lines.saturating_sub(max_lines);
    let mut kept: Vec<&str> = lines[start..].to_vec();
    let mut truncated_by = if start > 0 { Some("lines".into()) } else { None };

    // Enforce the byte cap from the tail.
    let total_bytes: usize = kept.iter().map(|l| l.len() + 1).sum();
    if total_bytes > max_bytes {
        let mut bytes = 0usize;
        let mut cut_from = 0;
        for (i, line) in kept.iter().enumerate().rev() {
            bytes += line.len() + 1;
            if bytes > max_bytes {
                cut_from = i + 1;
                truncated_by = Some("bytes".into());
                break;
            }
        }
        if cut_from >= kept.len() {
            cut_from = kept.len().saturating_sub(1);
        }
        kept = kept[cut_from..].to_vec();
    }

    let truncated = truncated_by.is_some();
    let content = kept.join("\n");
    let output_lines = kept.len();
    TruncationResult {
        output_bytes: content.len(),
        content,
        truncated,
        truncated_by,
        output_lines,
        total_lines,
        max_lines: Some(max_lines),
        max_bytes: Some(max_bytes),
        first_line_exceeds_limit: false,
        last_line_partial: false,
    }
}

/// Truncate a single (possibly very long) line for grep output.
pub fn truncate_line(line: &str, max: Option<usize>) -> (String, bool) {
    let max = max.unwrap_or(GREP_MAX_LINE_LENGTH);
    // Operate on chars, not bytes, to avoid splitting multi-byte sequences.
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max {
        return (line.to_string(), false);
    }
    let truncated: String = chars[..max].iter().collect();
    (format!("{truncated}…"), true)
}

/// Human-readable byte size, e.g. "12.3KB".
pub fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_truncates_by_lines() {
        let content = "a\nb\nc\nd\ne";
        let r = truncate_head(content, Some(2), Some(1024));
        assert_eq!(r.content, "a\nb");
        assert!(r.truncated);
        assert_eq!(r.truncated_by.as_deref(), Some("lines"));
    }

    #[test]
    fn tail_keeps_recent_lines() {
        let content = "a\nb\nc\nd\ne";
        let r = truncate_tail(content, Some(2), Some(1024));
        assert_eq!(r.content, "d\ne");
        assert!(r.truncated);
    }

    #[test]
    fn line_truncation_is_char_safe() {
        let s = "日本語".repeat(1000);
        let (t, truncated) = truncate_line(&s, Some(10));
        assert!(truncated);
        assert!(t.chars().count() <= 11);
    }

    #[test]
    fn head_does_not_inflate_line_count_for_trailing_newline() {
        // "a\nb\nc\n" is 3 lines; the trailing newline must not count as a 4th
        // (phantom) line that trips the line cap or inflates total_lines.
        let r = truncate_head("a\nb\nc\n", Some(2000), Some(1024));
        assert_eq!(r.total_lines, 3);
        assert_eq!(r.output_lines, 3);
        assert!(!r.truncated);
    }

    #[test]
    fn tail_does_not_inflate_line_count_for_trailing_newline() {
        // Command output almost always ends in '\n'. The phantom element must
        // not inflate total_lines, which bash reads for its "Showing lines X-Y
        // of N" truncation notice.
        let r = truncate_tail("a\nb\nc\n", Some(2000), Some(1024));
        assert_eq!(r.total_lines, 3);
        assert_eq!(r.output_lines, 3);
        assert!(!r.truncated);
    }

    #[test]
    fn tail_truncation_reports_an_exact_line_range() {
        // 10 lines + trailing newline, keep the last 3. Before the phantom fix
        // `total_lines` was 11 and the derived range was off by one. It must be
        // exactly lines 8-10 of 10 (bash computes start = total - output + 1).
        let r = truncate_tail("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n", Some(3), Some(1024));
        assert_eq!(r.total_lines, 10);
        assert_eq!(r.output_lines, 3);
        assert_eq!(r.content, "8\n9\n10");
        assert!(r.truncated);
    }
}
