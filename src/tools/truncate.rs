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

fn logical_line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.as_bytes().iter().filter(|&&b| b == b'\n').count();
    newlines + usize::from(!content.ends_with('\n'))
}

/// Truncate from the **head** (keep the first lines), used by read/write/ls.
/// Stops at whichever limit is hit first: `max_lines` or `max_bytes`.
pub fn truncate_head(
    content: &str,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let total_lines = logical_line_count(content);

    let first_line_bytes = content.find('\n').unwrap_or(content.len());
    if total_lines > 0 && first_line_bytes > max_bytes {
        return TruncationResult {
            first_line_exceeds_limit: true,
            total_lines,
            max_bytes: Some(max_bytes),
            max_lines: Some(max_lines),
            ..Default::default()
        };
    }

    let mut out = String::new();
    let mut bytes = 0usize;
    let mut output_lines = 0usize;
    let mut truncated_by: Option<String> = None;

    for line in content.split_terminator('\n') {
        let line_bytes = line.len() + usize::from(output_lines > 0);
        if output_lines + 1 > max_lines {
            truncated_by = Some("lines".into());
            break;
        }
        if bytes + line_bytes > max_bytes {
            truncated_by = Some("bytes".into());
            break;
        }
        if output_lines > 0 {
            out.push('\n');
        }
        out.push_str(line);
        bytes += line_bytes;
        output_lines += 1;
    }

    let truncated = truncated_by.is_some();
    TruncationResult {
        output_bytes: out.len(),
        content: out,
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

/// Truncate from the **tail** (keep the most recent output), used by bash.
pub fn truncate_tail(
    content: &str,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> TruncationResult {
    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let total_lines = logical_line_count(content);
    let end = if content.ends_with('\n') {
        content.len().saturating_sub(1)
    } else {
        content.len()
    };

    let bytes = content.as_bytes();
    let mut start = end;
    let mut line_end = end;
    let mut kept_lines = 0usize;
    let mut kept_bytes = 0usize;
    let mut bytes_limit_hit = false;

    while kept_lines < max_lines && kept_lines < total_lines {
        let mut line_start = line_end;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        let line_cost = line_end - line_start + usize::from(kept_lines > 0);
        if kept_lines == 0 && line_cost > max_bytes {
            bytes_limit_hit = true;
        }
        if kept_lines > 0 && kept_bytes + line_cost > max_bytes {
            bytes_limit_hit = true;
            break;
        }
        kept_bytes += line_cost;
        kept_lines += 1;
        start = line_start;
        if line_start == 0 {
            break;
        }
        line_end = line_start - 1;
    }

    let truncated_by = if bytes_limit_hit {
        Some("bytes".into())
    } else if kept_lines < total_lines {
        Some("lines".into())
    } else {
        None
    };

    let truncated = truncated_by.is_some();
    let content = content[start..end].to_string();
    TruncationResult {
        output_bytes: content.len(),
        content,
        truncated,
        truncated_by,
        output_lines: kept_lines,
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

    #[test]
    fn tail_reports_bytes_when_single_kept_line_exceeds_byte_limit() {
        // Bash tail truncation intentionally keeps an over-limit final line so
        // the caller can report that output was bounded by bytes. The optimized
        // scanner must preserve that notice even when the file has only one
        // logical line.
        let r = truncate_tail("abcdef", Some(10), Some(3));
        assert_eq!(r.content, "abcdef");
        assert!(r.truncated);
        assert_eq!(r.truncated_by.as_deref(), Some("bytes"));
    }

    #[test]
    fn head_allows_single_line_at_exact_byte_limit() {
        let r = truncate_head("abc", Some(10), Some(3));
        assert_eq!(r.content, "abc");
        assert_eq!(r.output_bytes, 3);
        assert!(!r.truncated);
    }

    #[test]
    fn tail_allows_single_line_at_exact_byte_limit() {
        let r = truncate_tail("abc", Some(10), Some(3));
        assert_eq!(r.content, "abc");
        assert_eq!(r.output_bytes, 3);
        assert!(!r.truncated);
    }

    #[test]
    fn tail_handles_single_empty_line_with_trailing_newline() {
        let r = truncate_tail("\n", Some(10), Some(10));
        assert_eq!(r.content, "");
        assert_eq!(r.output_lines, 1);
        assert_eq!(r.total_lines, 1);
        assert!(!r.truncated);
    }

    #[test]
    fn tail_preserves_trailing_empty_logical_line() {
        let r = truncate_tail("a\n\n", Some(2), Some(10));
        assert_eq!(r.content, "a\n");
        assert_eq!(r.output_lines, 2);
        assert_eq!(r.total_lines, 2);
        assert!(!r.truncated);
    }
}
