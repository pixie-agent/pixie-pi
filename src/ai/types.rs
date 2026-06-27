//! Core type system for the LLM abstraction layer.
//!
//! Mirrors `@earendil-works/pi-ai` types: messages, content blocks, usage,
//! models, and tool schemas. These types are the boundary between the agent
//! loop and the provider implementations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which wire protocol a model speaks. Beta targets only the Anthropic
/// Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    #[default]
    AnthropicMessages,
}

/// Why the assistant stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

/// Reasoning/thinking level for models that support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// A single block within an assistant message. Mirrors pi's
/// `TextContent | ThinkingContent | ToolCall`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        thinking_signature: String,
        #[serde(default)]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

impl ContentBlock {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn is_tool_call(&self) -> bool {
        matches!(self, ContentBlock::ToolCall { .. })
    }

    /// Returns (id, name, arguments) if this is a tool call.
    pub fn as_tool_call(&self) -> Option<(&str, &str, &Value)> {
        match self {
            ContentBlock::ToolCall { id, name, arguments } => Some((id, name, arguments)),
            _ => None,
        }
    }
}

/// A block within a user message. Mirrors `TextContent | ImageContent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserBlock {
    Text { text: String },
    Image { data: String, mime_type: String },
}

impl UserBlock {
    pub fn text(s: impl Into<String>) -> Self {
        UserBlock::Text { text: s.into() }
    }
}

/// Content returned from a tool execution. Mirrors the tool-result content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContent {
    Text { text: String },
    Image { data: String, mime_type: String },
}

impl ToolResultContent {
    pub fn text(s: impl Into<String>) -> Self {
        ToolResultContent::Text { text: s.into() }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A user-authored message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<UserBlock>,
    #[serde(default = "crate::ai::now_ms")]
    pub timestamp: i64,
}

impl UserMessage {
    pub fn new(content: Vec<UserBlock>) -> Self {
        Self {
            content,
            timestamp: crate::ai::now_ms(),
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![UserBlock::text(text)])
    }

    /// Concatenated plain-text view (images omitted).
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                UserBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A message produced by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default = "crate::ai::now_ms")]
    pub timestamp: i64,
}

impl AssistantMessage {
    pub fn empty() -> Self {
        Self {
            content: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            response_id: None,
            model: String::new(),
            provider: String::new(),
            timestamp: crate::ai::now_ms(),
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A tool-result message fed back to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContent>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default = "crate::ai::now_ms")]
    pub timestamp: i64,
}

/// The union of messages that make up a transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

impl From<UserMessage> for Message {
    fn from(m: UserMessage) -> Self {
        Message::User(m)
    }
}
impl From<AssistantMessage> for Message {
    fn from(m: AssistantMessage) -> Self {
        Message::Assistant(m)
    }
}
impl From<ToolResultMessage> for Message {
    fn from(m: ToolResultMessage) -> Self {
        Message::ToolResult(m)
    }
}

// ---------------------------------------------------------------------------
// Usage / cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub total: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost: Cost,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_tokens += other.total_tokens;
        self.cost.input += other.cost.input;
        self.cost.output += other.cost.output;
        self.cost.cache_read += other.cost.cache_read;
        self.cost.cache_write += other.cost.cache_write;
        self.cost.total += other.cost.total;
    }

    pub fn recompute_total(&mut self) {
        self.total_tokens = self.input + self.output + self.cache_read + self.cache_write;
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A resolved model definition used to drive provider requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub api: Api,
    /// Maximum output tokens the model can produce in one turn.
    pub max_tokens: usize,
    /// Context window size in tokens.
    pub context_window: usize,
    /// Base URL (without trailing slash) the provider POSTs to.
    pub base_url: String,
    /// Whether the model supports extended thinking / reasoning.
    pub reasoning: bool,
    /// Adaptive-thinking models select effort rather than a token budget.
    pub force_adaptive_thinking: bool,
    /// Whether passing `temperature` is supported.
    pub supports_temperature: bool,
    /// Pricing per 1M tokens (USD).
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: f64,
    pub cache_write_cost_per_mtok: f64,
}

impl Model {
    /// Compute and attach the dollar cost for a usage record.
    pub fn cost_for(&self, usage: &Usage) -> Cost {
        let per = 1_000_000.0;
        let input = usage.input as f64 / per * self.input_cost_per_mtok;
        let output = usage.output as f64 / per * self.output_cost_per_mtok;
        let cache_read = usage.cache_read as f64 / per * self.cache_read_cost_per_mtok;
        let cache_write = usage.cache_write as f64 / per * self.cache_write_cost_per_mtok;
        Cost {
            input,
            output,
            cache_read,
            cache_write,
            total: input + output + cache_read + cache_write,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool schema (sent to the model)
// ---------------------------------------------------------------------------

/// The schema fragment sent to the model describing an available tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
