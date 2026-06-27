//! `ls` tool — list directory contents.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES};
use crate::tools::util::{display_path, resolve_to_cwd};

const DEFAULT_ENTRY_LIMIT: usize = 500;

pub struct LsTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct LsInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            }
        })
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: LsInput = serde_json::from_value(args)?;
        let dir = resolve_to_cwd(input.path.as_deref().unwrap_or("."), &self.cwd);
        // Clamp to >= 1 (see find.rs): a `limit: 0` must not report
        // "(empty directory)" when the directory in fact has entries.
        let effective_limit = input.limit.unwrap_or(DEFAULT_ENTRY_LIMIT).max(1);
        let display = display_path(input.path.as_deref().unwrap_or("."), &self.cwd);

        if !dir.exists() {
            anyhow::bail!("Path not found: {display}");
        }
        if !dir.is_dir() {
            anyhow::bail!("Not a directory: {display}");
        }

        let mut entries: Vec<(String, bool)> = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("Cannot read directory: {e}"))?
            .filter_map(Result::ok)
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (name, is_dir)
            })
            .collect();
        entries.sort_by_key(|a| a.0.to_ascii_lowercase());

        let mut results: Vec<String> = Vec::new();
        let mut limit_reached = false;
        for (name, is_dir) in entries {
            if results.len() >= effective_limit {
                limit_reached = true;
                break;
            }
            results.push(if is_dir { format!("{name}/") } else { name });
        }

        if results.is_empty() {
            return Ok(ToolResult::text("(empty directory)"));
        }

        let raw = results.join("\n");
        // No line limit; entry count already caps rows.
        let trunc = truncate_head(&raw, Some(usize::MAX), None);
        let mut output = trunc.content;
        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!("{effective_limit} entries limit reached. Use limit={} for more", effective_limit * 2));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pi-ls-test-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run_ls(cwd: PathBuf, args: serde_json::Value) -> String {
        let tool = LsTool { cwd };
        let res = tool.execute(args, CancellationToken::new()).await.unwrap();
        match res.content.into_iter().next() {
            Some(crate::ai::types::ToolResultContent::Text { text }) => text,
            _ => panic!("expected text tool result"),
        }
    }

    #[tokio::test]
    async fn limit_zero_is_clamped_to_one_not_reported_empty() {
        // A degenerate `limit: 0` is clamped to 1 (see find.rs), so the first
        // entry is shown instead of the misleading "(empty directory)".
        let dir = scratch_dir("zero-limit");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();

        let out = run_ls(
            dir.clone(),
            serde_json::json!({ "path": dir.to_string_lossy(), "limit": 0 }),
        )
        .await;

        assert!(
            out.contains("a.txt"),
            "limit:0 should be clamped to 1 and show an entry, got: {out}"
        );
        assert!(
            !out.contains("empty directory"),
            "limit:0 must not report '(empty directory)' when entries exist: {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
