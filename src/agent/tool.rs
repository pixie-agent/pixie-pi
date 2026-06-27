//! Tool abstraction (`AgentTool` trait) and the permission gate hook.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ai::types::{ToolResultContent, ToolSchema};

/// How tool calls within a single assistant message are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Tool calls run one at a time.
    Sequential,
    /// Tool calls run concurrently (default).
    #[default]
    Parallel,
}

/// The structured result of executing a tool. `content` is what the model sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ToolResultContent>,
    /// Auxiliary payload attached by some tools (the bash truncation marker,
    /// the edit diff + first-changed line). Not consumed by the loop or sent to
    /// the model — only `content` is — but serialized with the result for
    /// debugging, so it is always emitted (never skipped).
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub terminate: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            details: Value::Null,
            terminate: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::text(message)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

/// A tool the agent can call. `prepare_arguments` lets a tool normalize raw
/// model arguments before validation (e.g. the edit tool accepting a legacy
/// `oldText`/`newText` shape).
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str;
    /// JSON Schema (object) describing the tool's parameters.
    fn input_schema(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    fn prepare_arguments(&self, args: Value) -> Value {
        args
    }
    /// Execute the tool. Throw on failure — the loop converts the error into
    /// an error tool result.
    async fn execute(
        &self,
        args: Value,
        cancel: CancellationToken,
    ) -> anyhow::Result<ToolResult>;

    /// The schema fragment sent to the model.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Lightweight required-field validation against a JSON schema. Ensures keys
/// listed in `required` are present (non-null) before a tool runs.
pub fn validate_required(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let obj = args.as_object().ok_or_else(|| "tool arguments must be an object".to_string())?;
    for r in required {
        if let Some(key) = r.as_str() {
            let present = obj
                .get(key)
                .map(|v| !v.is_null())
                .unwrap_or(false);
            if !present {
                return Err(format!("missing required parameter: {key}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Permission gate
// ---------------------------------------------------------------------------

/// Context handed to a [`ToolGate`] before a tool runs.
pub struct ToolCallContext<'a> {
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub args: &'a Value,
}

/// Result of a `beforeToolCall` check.
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
}

impl BeforeToolCallResult {
    pub fn allow() -> Self {
        Self {
            block: false,
            reason: None,
        }
    }
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            block: true,
            reason: Some(reason.into()),
        }
    }
}

/// Hook that may block a tool call (e.g. ask the user for permission).
#[async_trait]
pub trait ToolGate: Send + Sync {
    async fn before(&self, ctx: ToolCallContext<'_>) -> BeforeToolCallResult;
}

/// A gate that permits every call.
pub struct AllowAllGate;

#[async_trait]
impl ToolGate for AllowAllGate {
    async fn before(&self, _ctx: ToolCallContext<'_>) -> BeforeToolCallResult {
        BeforeToolCallResult::allow()
    }
}

pub fn allow_all_gate() -> Arc<dyn ToolGate> {
    Arc::new(AllowAllGate)
}
