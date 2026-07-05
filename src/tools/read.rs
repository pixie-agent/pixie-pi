//! `read` tool — read file contents (text or image).

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::ai::types::ToolResultContent;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};
use crate::tools::util::{display_path, resolve_read_path};

pub struct ReadTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default, alias = "file_path")]
    file_path: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

fn detect_image_mime(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".gif") {
        Some("image/gif")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else {
        None
    }
}

fn logical_line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.as_bytes().iter().filter(|&&b| b == b'\n').count();
    newlines + usize::from(!content.ends_with('\n'))
}

fn line_start_byte(content: &str, zero_based_line: usize) -> usize {
    if zero_based_line == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for (i, b) in content.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == zero_based_line {
                return i + 1;
            }
        }
    }
    content.len()
}

fn line_end_byte(content: &str, end_line_exclusive: usize, total_lines: usize) -> usize {
    if end_line_exclusive >= total_lines {
        return if content.ends_with('\n') {
            content.len().saturating_sub(1)
        } else {
            content.len()
        };
    }
    let mut seen = 0usize;
    for (i, b) in content.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == end_line_exclusive {
                return i;
            }
        }
    }
    content.len()
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp); images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files; continue with offset until complete."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: ReadInput = serde_json::from_value(args)?;
        let path_str = input
            .path
            .or(input.file_path)
            .ok_or_else(|| anyhow::anyhow!("read requires a 'path' parameter"))?;
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }
        let abs = resolve_read_path(&path_str, &self.cwd);
        let display = display_path(&path_str, &self.cwd);

        // Existence + readability.
        if !abs.exists() {
            anyhow::bail!("File not found: {display}");
        }
        if abs.is_dir() {
            anyhow::bail!("Path is a directory, not a file: {display}");
        }

        // Image branch.
        if let Some(mime) = detect_image_mime(&path_str) {
            let bytes = tokio::fs::read(&abs).await?;
            let b64 = base64_encode(&bytes);
            let note = format!("Read image file [{mime}]");
            return Ok(ToolResult {
                content: vec![
                    ToolResultContent::Text { text: note },
                    ToolResultContent::Image {
                        data: b64,
                        mime_type: mime.into(),
                    },
                ],
                details: Value::Null,
                terminate: false,
            });
        }

        let raw = tokio::fs::read_to_string(&abs).await?;
        let total_file_lines = logical_line_count(&raw);

        let start_line = input.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        if start_line >= total_file_lines {
            anyhow::bail!(
                "Offset {} is beyond end of file ({} lines total)",
                input.offset.unwrap_or(0),
                total_file_lines
            );
        }

        let selected: &str;
        let user_limited: Option<usize>;
        if let Some(raw_limit) = input.limit {
            // Clamp to >= 1 (see find.rs / ls.rs / grep.rs): a degenerate
            // `limit: 0` would otherwise read nothing and emit a continuation
            // hint that points back at the same offset (`Use offset=X` where X
            // is the line we just asked for) — a no-op the model can loop on.
            let limit = raw_limit.max(1);
            let end = (start_line + limit).min(total_file_lines);
            let start_byte = line_start_byte(&raw, start_line);
            let end_byte = line_end_byte(&raw, end, total_file_lines);
            selected = &raw[start_byte..end_byte];
            user_limited = Some(end - start_line);
        } else {
            let start_byte = line_start_byte(&raw, start_line);
            let end_byte = line_end_byte(&raw, total_file_lines, total_file_lines);
            selected = &raw[start_byte..end_byte];
            user_limited = None;
        }

        let trunc = truncate_head(selected, None, None);
        let start_display = start_line + 1;

        let output_text = if trunc.first_line_exceeds_limit {
            format!(
                "[Line {start_display} exceeds {} limit. Use bash: sed -n '{start_display}p' {display} | head -c {}]",
                format_size(DEFAULT_MAX_BYTES),
                DEFAULT_MAX_BYTES
            )
        } else if trunc.truncated {
            let end_display = start_display + trunc.output_lines - 1;
            let next = end_display + 1;
            let suffix = match trunc.truncated_by.as_deref() {
                Some("lines") => format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total_file_lines}. Use offset={next} to continue.]"
                ),
                _ => format!(
                    "\n\n[Showing lines {start_display}-{end_display} of {total_file_lines} ({} limit). Use offset={next} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                ),
            };
            format!("{}{suffix}", trunc.content)
        } else if let Some(n) = user_limited {
            if start_line + n < total_file_lines {
                let remaining = total_file_lines - (start_line + n);
                let next = start_line + n + 1;
                format!(
                    "{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]",
                    trunc.content
                )
            } else {
                trunc.content
            }
        } else {
            trunc.content
        };

        let _ = DEFAULT_MAX_LINES; // referenced in the description above
        Ok(ToolResult::text(output_text))
    }
}

/// Minimal base64 encoder (avoids pulling a base64 dependency).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn scratch_file(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("pi-read-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Run the read tool; returns `Ok(text)` on success or `Err(message)` on bail.
    async fn run_read(cwd: PathBuf, args: serde_json::Value) -> Result<String, String> {
        let tool = ReadTool { cwd };
        match tool.execute(args, CancellationToken::new()).await {
            Ok(res) => match res.content.into_iter().next() {
                Some(crate::ai::types::ToolResultContent::Text { text }) => Ok(text),
                _ => panic!("expected text tool result"),
            },
            Err(e) => Err(e.to_string()),
        }
    }

    #[tokio::test]
    async fn trailing_newline_does_not_inflate_line_count() {
        // A 3-line file with the usual trailing newline. With `limit: 2` the
        // remaining count must be 1 — the trailing newline must not be counted
        // as a phantom 4th line (previously reported "2 more lines").
        let path = scratch_file("l1\nl2\nl3\n");
        let cwd = std::env::temp_dir().to_path_buf();
        let out = run_read(
            cwd,
            serde_json::json!({ "path": path.to_string_lossy(), "limit": 2 }),
        )
        .await
        .unwrap();
        assert!(
            out.contains("[1 more lines in file. Use offset=3 to continue.]"),
            "expected exact remaining-count message, got:\n{out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn offset_beyond_end_uses_exact_line_count() {
        // Requesting offset 4 on a 3-line file must bail with "3 lines total",
        // not silently return a phantom line. (Previously offset 4 returned the
        // trailing-empty element and only offset 5 errored, as "4 lines total".)
        let path = scratch_file("l1\nl2\nl3\n");
        let cwd = std::env::temp_dir().to_path_buf();
        let err = run_read(
            cwd,
            serde_json::json!({ "path": path.to_string_lossy(), "offset": 4 }),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("3 lines total"),
            "expected '3 lines total' in error, got: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn limit_zero_is_clamped_to_one_not_an_empty_no_op() {
        // A degenerate `limit: 0` is clamped to 1 (see find.rs / ls.rs / grep.rs):
        // the first line is returned and the continuation hint advances to
        // offset 2, instead of an empty read whose hint points back at offset 1
        // (which the model could loop on forever).
        let path = scratch_file("l1\nl2\nl3\n");
        let cwd = std::env::temp_dir().to_path_buf();
        let out = run_read(
            cwd,
            serde_json::json!({ "path": path.to_string_lossy(), "limit": 0 }),
        )
        .await
        .unwrap();
        assert!(
            out.contains("l1"),
            "limit:0 must still return the first line, got: {out}"
        );
        assert!(
            !out.starts_with('\n'),
            "limit:0 must not return an empty/leading-blank read: {out:?}"
        );
        assert!(
            out.contains("Use offset=2"),
            "continuation hint must advance to offset 2, not point back at offset 1: {out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn full_read_of_trailing_newline_file_has_no_phantom_blank_line() {
        // Reading the whole file must not surface a phantom trailing blank line.
        let path = scratch_file("a\nb\nc\n");
        let cwd = std::env::temp_dir().to_path_buf();
        let out = run_read(cwd, serde_json::json!({ "path": path.to_string_lossy() }))
            .await
            .unwrap();
        assert!(
            !out.ends_with('\n'),
            "no trailing newline/blank line: {out:?}"
        );
        assert_eq!(out, "a\nb\nc");
        let _ = std::fs::remove_file(&path);
    }
}
