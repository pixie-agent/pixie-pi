//! `write` tool — create or overwrite a file.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::file_mutex::with_file_lock;
use crate::tools::util::{display_path, resolve_to_cwd};

pub struct WriteTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct WriteInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default, alias = "file_path")]
    file_path: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: WriteInput = serde_json::from_value(args)?;
        let path_str = input
            .path
            .or(input.file_path)
            .ok_or_else(|| anyhow::anyhow!("write requires a 'path' parameter"))?;
        let content = input.content.unwrap_or_default();
        let abs = resolve_to_cwd(&path_str, &self.cwd);
        let display = display_path(&path_str, &self.cwd);

        with_file_lock(&abs, async {
            if let Some(parent) = abs.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&abs, content.as_bytes()).await?;
            Ok::<(), anyhow::Error>(())
        })
        .await?;

        Ok(ToolResult::text(format!(
            "Successfully wrote {} bytes to {display}",
            content.len()
        )))
    }
}
