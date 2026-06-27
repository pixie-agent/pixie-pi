//! `edit` tool — exact text replacement with optional multi-edit.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::edit_diff::{
    apply_edits, detect_line_ending, generate_diff, normalize_to_lf, restore_line_endings,
    strip_bom, Edit,
};
use crate::tools::file_mutex::with_file_lock;
use crate::tools::util::{display_path, resolve_to_cwd};

pub struct EditTool {
    pub cwd: PathBuf,
}

/// Normalize raw tool arguments into `{ path, edits }`. Accepts:
/// - `{ path, edits: [{oldText,newText}] }`
/// - legacy `{ path, oldText, newText }`
/// - `edits` provided as a JSON string (some models do this).
fn prepare_arguments(args: Value) -> Value {
    let Some(obj) = args.as_object().cloned() else {
        return args;
    };
    let mut obj = obj;

    // Some models send `edits` as a JSON string.
    if let Some(Value::String(s)) = obj.get("edits").cloned() {
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&s) {
            obj.insert("edits".into(), Value::Array(a));
        }
    }

    // Legacy single oldText/newText → wrap into edits.
    let has_legacy = obj.get("oldText").is_some() && obj.get("newText").is_some();
    if has_legacy {
        let old = obj.remove("oldText").unwrap_or(Value::Null);
        let nw = obj.remove("newText").unwrap_or(Value::Null);
        let mut edits = match obj.get("edits").cloned() {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        let mut entry = serde_json::Map::new();
        entry.insert("oldText".into(), old);
        entry.insert("newText".into(), nw);
        edits.push(Value::Object(entry));
        obj.insert("edits".into(), Value::Array(edits));
    }
    Value::Object(obj)
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit. Do not include large unchanged regions just to connect distant changes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements, each matched against the original file (not incrementally). Non-overlapping.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text to replace; must be unique in the file" },
                            "newText": { "type": "string", "description": "Replacement text" }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        prepare_arguments(args)
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("file_path").and_then(|v| v.as_str()))
            .ok_or_else(|| anyhow::anyhow!("edit requires a 'path' parameter"))?
            .to_string();
        let edits_raw = args
            .get("edits")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("edit requires an 'edits' array with at least one replacement"))?;
        let edits: Vec<Edit> = serde_json::from_value(edits_raw)
            .map_err(|e| anyhow::anyhow!("edits must be an array of {{oldText,newText}}: {e}"))?;
        if edits.is_empty() {
            anyhow::bail!("edits must contain at least one replacement");
        }

        let abs = resolve_to_cwd(&path_str, &self.cwd);
        let display = display_path(&path_str, &self.cwd);

        let result = with_file_lock(&abs, async {
            if !abs.exists() {
                anyhow::bail!("Could not edit file: {display}. File does not exist.");
            }
            let raw = tokio::fs::read_to_string(&abs).await?;
            let (bom, text) = strip_bom(&raw);
            let ending = detect_line_ending(&text);
            let normalized = normalize_to_lf(&text);
            let (base, new_normalized) = apply_edits(&normalized, &edits, &display)?;
            let final_content = format!("{bom}{}", restore_line_endings(&new_normalized, ending));
            tokio::fs::write(&abs, final_content.as_bytes()).await?;
            let (diff, first_changed_line) = generate_diff(&base, &new_normalized, &display);
            Ok::<_, anyhow::Error>((edits.len(), diff, first_changed_line))
        })
        .await;

        let (count, diff, first_changed_line) = result?;
        let details = json!({
            "diff": diff,
            "firstChangedLine": first_changed_line,
        });
        let mut result = ToolResult::text(format!(
            "Successfully replaced {count} block(s) in {display}.\n\n{diff}"
        ));
        result.details = details;
        Ok(result)
    }
}
