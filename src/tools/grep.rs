//! `grep` tool — search file contents (gitignore-aware, in-process).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::truncate::{
    format_size, truncate_head, truncate_line, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH,
};
use crate::tools::util::{display_path, resolve_to_cwd};

const DEFAULT_MATCH_LIMIT: usize = 100;

pub struct GrepTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: Option<bool>,
    #[serde(default)]
    literal: Option<bool>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as a literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: GrepInput = serde_json::from_value(args)?;
        let effective_limit = input.limit.unwrap_or(DEFAULT_MATCH_LIMIT).max(1);
        let context = input.context.unwrap_or(0);

        let pattern = if input.literal.unwrap_or(false) {
            regex::escape(&input.pattern)
        } else {
            input.pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(input.ignore_case.unwrap_or(false))
            .build()
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;

        let search_path = resolve_to_cwd(input.path.as_deref().unwrap_or("."), &self.cwd);
        if !search_path.exists() {
            anyhow::bail!("Path not found: {}", display_path(input.path.as_deref().unwrap_or("."), &self.cwd));
        }
        let is_dir = search_path.is_dir();

        let glob_matcher = match input.glob.as_deref() {
            Some(g) => Some(
                globset::Glob::new(g)
                    .map_err(|e| anyhow::anyhow!("Invalid glob: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let files = collect_files(&search_path);
        let mut out_lines: Vec<String> = Vec::new();
        let mut match_count = 0usize;
        let mut match_limit_reached = false;
        let mut lines_truncated = false;

        'outer: for file in files {
            if let Some(gm) = &glob_matcher {
                let rel = file.strip_prefix(&search_path).unwrap_or(&file).to_string_lossy().to_string();
                let base = file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !gm.is_match(&rel) && !gm.is_match(&base) {
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            let lines: Vec<&str> = normalized.split('\n').collect();
            let display = display_for(&file, &search_path, is_dir);

            // Collect this file's matching line indices first (honoring the
            // global match limit) so context lines are emitted only once even
            // when matches are close together. The previous per-match loop
            // re-emitted overlapping context lines — and even re-emitted a
            // shared match line — whenever two matches sat within `2*context`
            // lines of each other.
            let mut match_indices: Vec<usize> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    match_count += 1;
                    match_indices.push(i);
                    if match_count > effective_limit {
                        match_limit_reached = true;
                        break;
                    }
                }
            }

            // We probe one match past the limit so "exactly N matches" is
            // distinguishable from "more than N" (the limit notice must not fire
            // when nothing was actually held back). That extra match lands in
            // `match_indices`; if it pushed us over the global limit, drop it —
            // and any surplus in this file — so exactly `effective_limit` matches
            // are emitted total. `match_count - match_indices.len()` is the number
            // of matches already emitted from earlier files.
            if match_limit_reached {
                let emitted_before = match_count - match_indices.len();
                match_indices.truncate(effective_limit.saturating_sub(emitted_before));
            }

            if context == 0 {
                for &i in &match_indices {
                    let (truncated, was) = truncate_line(lines[i], None);
                    if was {
                        lines_truncated = true;
                    }
                    out_lines.push(format!("{}:{}: {}", display, i + 1, truncated));
                }
            } else if !match_indices.is_empty() {
                let is_match: HashSet<usize> = match_indices.iter().copied().collect();
                let n = lines.len();
                // Merge the [i-context, i+context] windows around each match
                // into maximal disjoint runs (overlapping or adjacent windows
                // coalesce), mirroring `grep -C`. Emit each covered line once —
                // matches with ':', context with '-' — with a "--" separator
                // between disjoint groups.
                let mut groups: Vec<(usize, usize)> = Vec::new();
                for &i in &match_indices {
                    let start = i.saturating_sub(context);
                    let end = (i + context).min(n.saturating_sub(1));
                    match groups.last_mut() {
                        Some((_, prev_end)) if start <= *prev_end + 1 => {
                            *prev_end = (*prev_end).max(end);
                        }
                        _ => groups.push((start, end)),
                    }
                }
                for (gi, (start, end)) in groups.iter().enumerate() {
                    if gi > 0 {
                        out_lines.push("--".to_string());
                    }
                    for (offset, line) in lines[*start..=*end].iter().enumerate() {
                        let c = start + offset;
                        let (truncated, was) = truncate_line(line, None);
                        if was {
                            lines_truncated = true;
                        }
                        if is_match.contains(&c) {
                            out_lines.push(format!("{}:{}: {}", display, c + 1, truncated));
                        } else {
                            out_lines.push(format!("{}-{}- {}", display, c + 1, truncated));
                        }
                    }
                }
            }

            if match_limit_reached {
                break 'outer;
            }
        }

        if match_count == 0 {
            return Ok(ToolResult::text("No matches found"));
        }

        let raw = out_lines.join("\n");
        let trunc = truncate_head(&raw, None, None);
        let mut output = trunc.content;
        let mut notices: Vec<String> = Vec::new();
        if match_limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
        }
        if trunc.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        Ok(ToolResult::text(output))
    }
}

/// Walk `root` (gitignore-aware, including hidden files) and return file paths.
fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
        return files;
    }
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(entry.into_path());
        }
    }
    files
}

fn display_for(file: &Path, search_root: &Path, is_dir: bool) -> String {
    if is_dir {
        if let Ok(rel) = file.strip_prefix(search_root) {
            let s = rel.to_string_lossy().replace('\\', "/");
            if s.is_empty() {
                return file
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
            }
            return s;
        }
    }
    file.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pi-grep-test-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run_grep(cwd: PathBuf, args: serde_json::Value) -> String {
        let tool = GrepTool { cwd };
        let res = tool.execute(args, CancellationToken::new()).await.unwrap();
        match res.content.into_iter().next() {
            Some(crate::ai::types::ToolResultContent::Text { text }) => text,
            _ => panic!("expected text tool result"),
        }
    }

    #[tokio::test]
    async fn context_merges_overlapping_windows_no_duplicates() {
        // Two matches on adjacent lines; with context=1 their windows overlap.
        // The previous per-match loop emitted each covered line twice (a match
        // line re-appeared as a "-" context line for its neighbor). The merged
        // output must list every line exactly once.
        let dir = scratch_dir("adjacent");
        std::fs::write(dir.join("f.txt"), "a\nb\nMATCH\nMATCH\nc\nd\n").unwrap();

        let out = run_grep(
            dir.clone(),
            serde_json::json!({ "pattern": "MATCH", "path": dir.to_string_lossy(), "context": 1 }),
        )
        .await;

        let body: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            body,
            vec![
                "f.txt-2- b",
                "f.txt:3: MATCH",
                "f.txt:4: MATCH",
                "f.txt-5- c",
            ],
            "adjacent matches should merge into one group with no duplicates"
        );
        // Coalesced groups carry no separator.
        assert!(!out.contains("\n--\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn context_separates_disjoint_groups() {
        // Two matches far apart produce two disjoint groups joined by "--",
        // each line emitted once.
        let dir = scratch_dir("disjoint");
        std::fs::write(dir.join("f.txt"), "MATCH\nx\nx\nx\nx\nx\nx\nx\nMATCH\n").unwrap();

        let out = run_grep(
            dir.clone(),
            serde_json::json!({ "pattern": "MATCH", "path": dir.to_string_lossy(), "context": 1 }),
        )
        .await;

        assert!(
            out.contains("\n--\n"),
            "disjoint groups should be separated by --\n{out}"
        );
        // Each match line appears exactly once.
        assert_eq!(out.matches(":1:").count(), 1);
        assert_eq!(out.matches(":9:").count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limit_not_reported_when_matches_equal_limit_exactly() {
        // Exactly `limit` matches exist → all are shown, and the "limit reached"
        // notice must NOT fire (nothing was held back).
        let dir = scratch_dir("at-limit");
        std::fs::write(dir.join("f.txt"), "MATCH\nMATCH\nMATCH\n").unwrap();

        let out = run_grep(
            dir.clone(),
            serde_json::json!({ "pattern": "MATCH", "path": dir.to_string_lossy(), "limit": 3 }),
        )
        .await;

        assert_eq!(out.matches("f.txt:").count(), 3, "all 3 matches shown");
        assert!(
            !out.contains("limit reached"),
            "no false-positive limit notice\n{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limit_reported_and_capped_when_matches_exceed_limit() {
        // One more match than the limit → notice fires and exactly `limit`
        // matches are emitted (no off-by-one in the straddling file).
        let dir = scratch_dir("over-limit");
        std::fs::write(dir.join("f.txt"), "MATCH\nMATCH\nMATCH\nMATCH\n").unwrap();

        let out = run_grep(
            dir.clone(),
            serde_json::json!({ "pattern": "MATCH", "path": dir.to_string_lossy(), "limit": 3 }),
        )
        .await;

        assert_eq!(out.matches("f.txt:").count(), 3, "exactly 3 matches shown");
        assert!(
            out.contains("limit reached"),
            "truncation notice present\n{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limit_straddling_two_files_emits_exactly_limit() {
        // The limit is crossed inside the second file. Exactly `limit` matches
        // must be emitted across both files — this is the case the per-file
        // truncation math is really for.
        let dir = scratch_dir("straddle");
        std::fs::write(dir.join("a.txt"), "MATCH\nMATCH\n").unwrap();
        std::fs::write(dir.join("b.txt"), "MATCH\nMATCH\n").unwrap();

        let out = run_grep(
            dir.clone(),
            serde_json::json!({ "pattern": "MATCH", "path": dir.to_string_lossy(), "limit": 3 }),
        )
        .await;

        assert_eq!(
            out.matches(".txt:").count(),
            3,
            "exactly 3 matches across both files\n{out}"
        );
        assert!(
            out.contains("limit reached"),
            "truncation notice present\n{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
