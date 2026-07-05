//! `find` tool — find files by glob pattern (gitignore-aware, in-process).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
use crate::tools::util::{display_path, resolve_to_cwd};

const DEFAULT_RESULT_LIMIT: usize = 1000;

pub struct FindTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct FindInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: FindInput = serde_json::from_value(args)?;
        // Clamp to >= 1: a degenerate `limit: 0` would otherwise cap the result
        // set to zero and report "No files found" even when matches exist. grep
        // already does this; ls mirrors the same guard for consistency.
        let effective_limit = input.limit.unwrap_or(DEFAULT_RESULT_LIMIT).max(1);
        let search_path = resolve_to_cwd(input.path.as_deref().unwrap_or("."), &self.cwd);
        if !search_path.exists() {
            anyhow::bail!(
                "Path not found: {}",
                display_path(input.path.as_deref().unwrap_or("."), &self.cwd)
            );
        }

        // A glob like "*.ts" should match by basename; "**/*.json" matches the
        // full relative path. Test both.
        let matcher = globset::Glob::new(&input.pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?
            .compile_matcher();

        let mut matches: Vec<String> = Vec::new();
        // Collect up to one past the limit so we can tell "exactly N matches"
        // (no truncation) from "more than N" (genuine truncation). Without the
        // +1 probe, a result set of exactly `effective_limit` files would spuriously
        // report "limit reached" even though nothing was held back.
        let mut result_limit_reached = false;
        if search_path.is_file() {
            push_match(&search_path, &search_path, &matcher, &mut matches);
        } else {
            let walker = ignore::WalkBuilder::new(&search_path)
                .hidden(false)
                .git_ignore(true)
                .git_global(true)
                .parents(true)
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                push_match(&entry.into_path(), &search_path, &matcher, &mut matches);
                if matches.len() > effective_limit {
                    result_limit_reached = true;
                    break;
                }
            }
        }

        // Drop the surplus probe entry; the displayed set is identical to the
        // pre-fix behavior (the first `effective_limit` matches in walk order).
        matches.truncate(effective_limit);
        matches.sort();

        if matches.is_empty() {
            return Ok(ToolResult::text("No files found matching pattern"));
        }

        let raw = matches.join("\n");
        let trunc = truncate_head(&raw, None, None);
        let mut output = trunc.content;
        let mut notices: Vec<String> = Vec::new();
        if result_limit_reached {
            notices.push(format!(
                "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
        }
        if trunc.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        Ok(ToolResult::text(output))
    }
}

fn push_match(
    file: &Path,
    search_path: &Path,
    matcher: &globset::GlobMatcher,
    matches: &mut Vec<String>,
) {
    let rel_path = file.strip_prefix(search_path).unwrap_or(file);
    let rel = if rel_path.as_os_str().is_empty() {
        file.file_name()
            .map(|n| n.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    } else {
        rel_path.to_string_lossy().replace('\\', "/")
    };
    let base = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if matcher.is_match(&rel) || matcher.is_match(&base) {
        matches.push(rel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pi-find-test-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run_find(cwd: PathBuf, args: serde_json::Value) -> String {
        let tool = FindTool { cwd };
        let res = tool.execute(args, CancellationToken::new()).await.unwrap();
        match res.content.into_iter().next() {
            Some(crate::ai::types::ToolResultContent::Text { text }) => text,
            _ => panic!("expected text tool result"),
        }
    }

    #[tokio::test]
    async fn limit_not_reported_when_results_equal_limit_exactly() {
        // Exactly `limit` files match → all are shown, and the "limit reached"
        // notice must NOT fire (nothing was held back).
        let dir = scratch_dir("at-limit");
        for i in 1..=3 {
            std::fs::write(dir.join(format!("match{i}.txt")), "x").unwrap();
        }

        let out = run_find(
            dir.clone(),
            serde_json::json!({ "pattern": "match*.txt", "path": dir.to_string_lossy(), "limit": 3 }),
        )
        .await;

        assert_eq!(out.matches(".txt").count(), 3, "all 3 results shown");
        assert!(
            !out.contains("limit reached"),
            "no false-positive limit notice\n{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limit_reported_and_capped_when_results_exceed_limit() {
        // One more result than the limit → notice fires and exactly `limit`
        // results are emitted.
        let dir = scratch_dir("over-limit");
        for i in 1..=4 {
            std::fs::write(dir.join(format!("match{i}.txt")), "x").unwrap();
        }

        let out = run_find(
            dir.clone(),
            serde_json::json!({ "pattern": "match*.txt", "path": dir.to_string_lossy(), "limit": 3 }),
        )
        .await;

        assert_eq!(out.matches(".txt").count(), 3, "exactly 3 results shown");
        assert!(
            out.contains("limit reached"),
            "truncation notice present\n{out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limit_zero_is_clamped_to_one_not_reported_empty() {
        // A degenerate `limit: 0` is clamped to 1 (matching grep), so at least
        // one match is shown instead of the misleading "No files found".
        let dir = scratch_dir("zero-limit");
        std::fs::write(dir.join("match.txt"), "x").unwrap();

        let out = run_find(
            dir.clone(),
            serde_json::json!({ "pattern": "match*.txt", "path": dir.to_string_lossy(), "limit": 0 }),
        )
        .await;

        assert!(
            out.contains("match.txt"),
            "limit:0 should be clamped to 1 and show the match, got: {out}"
        );
        assert!(
            !out.contains("No files found"),
            "limit:0 must not report 'No files found' when a file matches: {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn searching_a_single_file_returns_its_name() {
        let dir = scratch_dir("single-file");
        let path = dir.join("match.txt");
        std::fs::write(&path, "x").unwrap();

        let out = run_find(
            dir.clone(),
            serde_json::json!({ "pattern": "*.txt", "path": path.to_string_lossy() }),
        )
        .await;

        assert_eq!(out, "match.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
