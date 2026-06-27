//! Agent context snapshot and event protocol.

use std::sync::Arc;

use serde_json::Value;

use crate::ai::stream::AssistantMessageEvent;
use crate::ai::types::{AssistantMessage, Message, Usage};
use crate::agent::tool::AgentTool;

/// A snapshot of agent state passed into the loop. `AgentMessage` here is just
/// [`Message`] (no custom app messages in this port).
#[derive(Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl AgentContext {
    pub fn new(system_prompt: String, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        Self {
            system_prompt,
            messages: Vec::new(),
            tools,
        }
    }

    /// Look up a tool by name.
    pub fn find_tool(&self, name: &str) -> Option<&Arc<dyn AgentTool>> {
        self.tools.iter().find(|t| t.name() == name)
    }
}

/// Events emitted by the agent loop for UI updates. Mirrors pi's `AgentEvent`.
#[derive(Debug, Clone, serde::Serialize)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<crate::ai::types::ToolResultMessage>,
    },
    MessageStart(Message),
    /// Fine-grained streaming event for an in-progress assistant message. The
    /// renderer applies deltas incrementally (no full-message clone needed).
    MessageUpdate {
        event: AssistantMessageEvent,
    },
    MessageEnd(Message),
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
    /// Aggregated token usage for the most recent assistant turn.
    Usage(Usage),
    /// A fatal agent error (loop could not complete).
    Error(String),
}
