//! `ls` tool — list directory contents.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
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

        let (results, limit_reached) = collect_limited_entries(&dir, effective_limit)?;

        if results.is_empty() {
            return Ok(ToolResult::text("(empty directory)"));
        }

        let raw = results.join("\n");
        // No line limit; entry count already caps rows.
        let trunc = truncate_head(&raw, Some(usize::MAX), None);
        let mut output = trunc.content;
        let mut notices: Vec<String> = Vec::new();
        if limit_reached {
            notices.push(format!(
                "{effective_limit} entries limit reached. Use limit={} for more",
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

#[derive(Debug, Eq, PartialEq)]
struct DirEntryRow {
    sort_key: String,
    name: String,
    is_dir: bool,
}

impl DirEntryRow {
    fn new(name: String, is_dir: bool) -> Self {
        Self {
            sort_key: name.to_ascii_lowercase(),
            name,
            is_dir,
        }
    }

    fn display(self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name
        }
    }
}

impl Ord for DirEntryRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key
            .cmp(&other.sort_key)
            .then_with(|| self.name.cmp(&other.name))
            .then_with(|| self.is_dir.cmp(&other.is_dir))
    }
}

impl PartialOrd for DirEntryRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn collect_limited_entries(
    dir: &PathBuf,
    effective_limit: usize,
) -> anyhow::Result<(Vec<String>, bool)> {
    let probe_limit = effective_limit.saturating_add(1);
    let mut heap: BinaryHeap<DirEntryRow> = BinaryHeap::with_capacity(probe_limit);

    for entry in
        std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("Cannot read directory: {e}"))?
    {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let row = DirEntryRow::new(name, is_dir);

        if heap.len() < probe_limit {
            heap.push(row);
        } else if heap.peek().is_some_and(|largest| row < *largest) {
            let mut largest = heap.peek_mut().expect("heap has at least one entry");
            *largest = row;
        }
    }

    let limit_reached = heap.len() > effective_limit;
    if limit_reached {
        heap.pop();
    }

    let mut entries = heap.into_vec();
    entries.sort();
    Ok((
        entries.into_iter().map(DirEntryRow::display).collect(),
        limit_reached,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-ls-test-{label}-{}", uuid::Uuid::new_v4()));
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

    #[tokio::test]
    async fn limit_keeps_first_entries_in_sorted_order_and_reports_more() {
        let dir = scratch_dir("sorted-limit");
        std::fs::write(dir.join("z.txt"), "x").unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("m.txt"), "x").unwrap();

        let out = run_ls(
            dir.clone(),
            serde_json::json!({ "path": dir.to_string_lossy(), "limit": 2 }),
        )
        .await;

        assert!(
            out.starts_with("a.txt\nm.txt"),
            "sorted limited output: {out}"
        );
        assert!(
            !out.contains("z.txt"),
            "limit should cap visible rows: {out}"
        );
        assert!(
            out.contains("2 entries limit reached"),
            "limit notice present: {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
