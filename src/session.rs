//! Agent session: holds the live transcript, model, tools, and credentials,
//! drives the agent loop, and persists the transcript as JSONL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::agent_loop::{agent_loop, AgentEventStream, AgentLoopConfig};
use crate::agent::context::AgentContext;
use crate::agent::tool::{AgentTool, ExecutionMode, ToolGate, allow_all_gate};
use crate::ai::anthropic::{stream_simple, LlmContext, SimpleStreamOptions};
use crate::ai::stream::{AssistantMessageEvent, AssistantStream};
use crate::ai::types::{Message, Model, ThinkingLevel, Usage, UserBlock, UserMessage};
use futures::StreamExt;

/// Rough characters-per-token estimate for budgeting/compaction.
const CHARS_PER_TOKEN: usize = 4;

pub struct AgentSession {
    pub cwd: PathBuf,
    pub messages: Vec<Message>,
    pub system_prompt: String,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub client: reqwest::Client,
    pub gate: Arc<dyn ToolGate>,
    pub api_key: Option<String>,
    pub auth_token: Option<String>,
    pub session_file: Option<PathBuf>,
    pub total_usage: Usage,
    /// Override the model's default output-token cap, if set.
    pub max_tokens: Option<usize>,
    /// Attach Anthropic prompt-caching markers. Default true (`--no-cache` flips it).
    pub cache_control: bool,
    /// Override the model used for background compaction summaries. `None`
    /// (default) resolves via [`Self::default_compact_model`] — the cheapest
    /// tier (haiku), or `PIXIE_PI_COMPACT_MODEL`. Pinned per-session in tests.
    pub compact_model: Option<Model>,
}

impl AgentSession {
    /// Construct a session from its core parts, with sensible defaults for
    /// token caps, caching, the allow-all gate, and an empty transcript.
    pub fn new(
        cwd: PathBuf,
        system_prompt: String,
        model: Model,
        thinking: ThinkingLevel,
        tools: Vec<Arc<dyn AgentTool>>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            cwd,
            messages: Vec::new(),
            system_prompt,
            model,
            thinking,
            tools,
            client,
            gate: default_gate(),
            api_key: None,
            auth_token: None,
            session_file: None,
            total_usage: Usage::default(),
            max_tokens: None,
            cache_control: true,
            compact_model: None,
        }
    }

    /// Build the streaming options for the current model + thinking level.
    pub fn stream_options(&self) -> SimpleStreamOptions {
        SimpleStreamOptions {
            api_key: self.api_key.clone(),
            auth_token: self.auth_token.clone(),
            max_tokens: self.max_tokens,
            temperature: None,
            reasoning: if self.model.reasoning {
                Some(self.thinking)
            } else {
                None
            },
            thinking_budget_tokens: None,
            cache_control: self.cache_control,
            timeout_ms: None,
        }
    }

    /// Start an agent run for the given prompt messages.
    pub fn run(&self, prompts: Vec<Message>, cancel: CancellationToken) -> AgentEventStream {
        let ctx = AgentContext {
            system_prompt: self.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
        };
        let config = AgentLoopConfig {
            client: self.client.clone(),
            model: self.model.clone(),
            options: self.stream_options(),
            tool_execution: ExecutionMode::Parallel,
            gate: Some(self.gate.clone()),
        };
        agent_loop(ctx, config, prompts, cancel)
    }

    /// Estimated token count of the current transcript.
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.messages)
    }

    /// Token-budget fraction used (0.0–1.0+).
    pub fn context_usage(&self) -> f64 {
        self.estimated_tokens() as f64 / self.model.context_window.max(1) as f64
    }

    /// Compaction budget: ~80% of the context window.
    fn compaction_budget(&self) -> usize {
        (self.model.context_window as f64 * 0.8) as usize
    }

    /// Resolve the model for background compaction summaries (model tiering):
    /// a per-session override, then the `PIXIE_PI_COMPACT_MODEL` env var, then the
    /// cheapest tier (haiku) for cost, finally the session's main model. Using a
    /// cheap tier for summaries keeps long sessions inexpensive regardless of
    /// the (possibly opus-tier) main model.
    fn default_compact_model(&self) -> Model {
        let registry = crate::ai::builtin_models();
        if let Ok(pattern) = std::env::var("PIXIE_PI_COMPACT_MODEL") {
            let pattern = pattern.trim();
            if !pattern.is_empty() {
                if let Some(m) = crate::ai::resolve_model(&registry, pattern) {
                    return m;
                }
            }
        }
        registry
            .iter()
            .find(|m| m.id.contains("haiku"))
            .cloned()
            .unwrap_or_else(|| self.model.clone())
    }

    /// True when the transcript exceeds the compaction budget.
    pub fn should_compact(&self) -> bool {
        self.estimated_tokens() > self.compaction_budget()
    }

    /// Index of the first message to KEEP after compaction. We only cut at a
    /// plain user-message boundary, so the remaining transcript still starts
    /// with a user turn and no tool_use ↔ tool_result pair is severed — keeping
    /// it valid for the Anthropic API. Returns 0 when nothing need be dropped.
    fn compaction_cut(&self) -> usize {
        let budget = self.compaction_budget();
        let mut cut = 0;
        loop {
            let remaining = &self.messages[cut..];
            if remaining.len() <= 4 || estimate_tokens(remaining) <= budget {
                break;
            }
            // Next user boundary within `remaining` (skip index 0 — cutting there
            // would drop everything). `rel` is relative to `remaining[1..]`.
            match remaining[1..]
                .iter()
                .position(|m| matches!(m, Message::User(_)))
            {
                Some(rel) => cut += rel + 1,
                None => break, // No safe boundary; stop rather than corrupt.
            }
        }
        cut
    }

    /// Summarize a dropped prefix of the transcript via the model. Returns
    /// `None` on any failure (or empty result) so the caller always has a clean
    /// fallback. Uses a short, no-thinking, no-cache, time-boxed request — a
    /// summary must never hang or bankrupt the turn it runs in.
    async fn summarize(&self, messages: &[Message]) -> Option<String> {
        if messages.is_empty() {
            return None;
        }
        let transcript = transcript_for_summary(messages);
        if transcript.trim().is_empty() {
            return None;
        }
        let prompt = format!(
            "You are compacting a coding-agent conversation to save context. Summarize everything \
             below, preserving: the user's original goal and constraints; key decisions and their \
             rationale; every file read or modified and what was done to it; important findings, \
             errors, and their resolution; and any open problems or planned next steps. Be concise \
             and factual — this summary replaces the earlier messages, so include anything the \
             agent still needs to continue. No preamble or commentary.\n\n--- BEGIN TRANSCRIPT ---\n\
             {transcript}\n--- END TRANSCRIPT ---"
        );
        let user = vec![Message::User(UserMessage::text(prompt))];
        let tools: Vec<crate::ai::types::ToolSchema> = Vec::new();
        let llm = LlmContext {
            system_prompt: Some("You compact agent conversation transcripts."),
            messages: &user,
            tools: &tools,
        };
        let options = SimpleStreamOptions {
            api_key: self.api_key.clone(),
            auth_token: self.auth_token.clone(),
            max_tokens: Some(1024),
            temperature: None,
            reasoning: None,
            thinking_budget_tokens: None,
            cache_control: false,
            timeout_ms: Some(60_000),
        };
        // Use the (cheap) compaction-tier model for the summary, not the main
        // model — see default_compact_model.
        let model = self
            .compact_model
            .clone()
            .unwrap_or_else(|| self.default_compact_model());
        let stream = stream_simple(&self.client, &model, &llm, &options, CancellationToken::new());
        collect_assistant_text(stream).await
    }

    /// Compaction: summarize the oldest messages (cut at a user boundary) and
    /// replace them with the summary, keeping the recent tail. Falls back to a
    /// plain "messages dropped" note if the model is unavailable or fails, so
    /// compaction always reduces the transcript and never hard-fails. Returns
    /// the number of messages dropped.
    pub async fn compact(&mut self) -> usize {
        let cut = self.compaction_cut();
        if cut == 0 {
            return 0;
        }
        let dropped: Vec<Message> = self.messages[..cut].to_vec();
        let summary = self.summarize(&dropped).await;
        self.apply_compaction(cut, summary);
        cut
    }

    /// Pure apply-step: drop `[0..cut)`, then fold `summary` (or a fallback
    /// note) into the new first user message so roles stay valid (two
    /// consecutive user roles are rejected by the API).
    fn apply_compaction(&mut self, cut: usize, summary: Option<String>) {
        let prefix = match summary {
            Some(ref s) => format!(
                "[Compaction summary — earlier messages were summarized to save context. Treat this \
                 as background; do not re-read these files just to verify it.]\n\n{s}"
            ),
            None => "[context compacted: earlier messages were dropped to stay within the token budget]"
                .into(),
        };
        self.messages.drain(0..cut);
        match self.messages.first_mut() {
            Some(Message::User(u)) => u.content.insert(0, UserBlock::text(prefix)),
            _ => self.messages.insert(0, Message::User(UserMessage::text(prefix))),
        }
    }

    /// Persist the transcript to the session JSONL file (if any).
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.session_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for msg in &self.messages {
            let line = serde_json::to_string(msg).unwrap_or_default();
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(path, out)
    }

    /// Load a transcript from a JSONL session file.
    pub fn load(path: &Path) -> std::io::Result<Vec<Message>> {
        let content = std::fs::read_to_string(path)?;
        let mut messages = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Message>(line) {
                Ok(m) => messages.push(m),
                Err(_) => continue,
            }
        }
        Ok(messages)
    }

    /// Accumulate usage from a turn into the session total.
    pub fn add_usage(&mut self, usage: &Usage) {
        self.total_usage.add(usage);
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }
}

/// Builder with sensible defaults.
pub fn default_gate() -> Arc<dyn ToolGate> {
    allow_all_gate()
}

/// Sentinel used to attach a compacted-note marker; exposed for tests.
pub fn compact_marker() -> Value {
    Value::String("compacted".into())
}

/// Rough token estimate for a slice of messages (~4 chars/token). Extracted from
/// the old `estimated_tokens` so `compaction_cut` can budget an arbitrary tail.
fn estimate_tokens(messages: &[Message]) -> usize {
    let chars: usize = messages
        .iter()
        .map(|m| match m {
            Message::User(u) => u.text_content().len(),
            Message::Assistant(a) => a.text_content().len(),
            Message::ToolResult(t) => t
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::ai::types::ToolResultContent::Text { text } => Some(text.len()),
                    _ => None,
                })
                .sum::<usize>(),
        })
        .sum();
    chars / CHARS_PER_TOKEN
}

/// Render a transcript slice to plain text for the summarizer: user/assistant
/// text plus tool calls and their results, so the summary captures what was
/// actually done — not just prose.
fn transcript_for_summary(messages: &[Message]) -> String {
    use crate::ai::types::{ContentBlock, ToolResultContent};
    let mut out = String::new();
    for m in messages {
        match m {
            Message::User(u) => {
                out.push_str("User: ");
                out.push_str(&u.text_content());
                out.push('\n');
            }
            Message::Assistant(a) => {
                let text = a.text_content();
                if !text.trim().is_empty() {
                    out.push_str("Assistant: ");
                    out.push_str(&text);
                    out.push('\n');
                }
                for block in &a.content {
                    if let ContentBlock::ToolCall { name, arguments, .. } = block {
                        out.push_str(&format!(
                            "  [Assistant calls tool {name} with: {arguments}]\n"
                        ));
                    }
                }
            }
            Message::ToolResult(t) => {
                let text = t
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let tag = if t.is_error { "ERROR result" } else { "result" };
                out.push_str(&format!("  [Tool {tag} for {}: {text}]\n", t.tool_name));
            }
        }
    }
    out
}

/// Drain a provider stream to its final assistant message and return its text.
/// Used by the one-shot compaction summary call (no tool loop, no streaming UI).
async fn collect_assistant_text(mut stream: AssistantStream) -> Option<String> {
    let mut final_msg: Option<crate::ai::types::AssistantMessage> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { message, .. } => final_msg = Some(message),
            _ => {}
        }
    }
    let msg = final_msg?;
    if msg.error_message.is_some() {
        return None;
    }
    let text = msg.text_content();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{AssistantMessage, Message, UserMessage};

    fn session_with(messages: Vec<Message>) -> AgentSession {
        let mut model = crate::ai::builtin_models()[0].clone();
        model.context_window = 1; // tiny so compaction always wants to drop
        let session = AgentSession::new(
            std::path::PathBuf::from("."),
            "sys".into(),
            model,
            crate::ai::ThinkingLevel::Off,
            vec![],
            reqwest::Client::new(),
        );
        AgentSession {
            messages,
            ..session
        }
    }

    #[test]
    fn default_compact_model_uses_the_cheapest_tier() {
        // With no per-session override and (normally) no PIXIE_PI_COMPACT_MODEL env var,
        // the summary model should default to the cheapest tier (haiku).
        let model = crate::ai::builtin_models()[0].clone();
        let session = AgentSession::new(
            std::path::PathBuf::from("."),
            "sys".into(),
            model,
            crate::ai::ThinkingLevel::Off,
            vec![],
            reqwest::Client::new(),
        );
        let cm = session.default_compact_model();
        assert!(cm.id.contains("haiku"), "default compact model should be haiku: {}", cm.id);
    }

    #[test]
    fn compaction_cut_drops_oldest_exchange_at_user_boundary() {
        // Three exchanges: U A U A U A. With a tiny context window the cut must
        // drop at least the first exchange and land on a user boundary, leaving
        // a valid alternating transcript.
        let msgs = vec![
            Message::User(UserMessage::text("first prompt")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("second prompt")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("third prompt")),
            Message::Assistant(AssistantMessage::empty()),
        ];
        let s = session_with(msgs);
        let cut = s.compaction_cut();
        assert!(cut >= 2, "should cut at least the first exchange, got {cut}");
        let remaining = &s.messages[cut..];
        assert!(matches!(remaining.first(), Some(Message::User(_))), "starts with user");
        assert!(matches!(remaining.get(1), Some(Message::Assistant(_))), "alternates");
    }

    #[test]
    fn compaction_cut_refuses_to_sever_tool_chain() {
        // One user prompt then an assistant/tool chain with no later user
        // boundary — the cut must be 0 (stop) rather than orphan a tool result.
        use crate::ai::types::ToolResultMessage;
        let msgs = vec![
            Message::User(UserMessage::text("only prompt")),
            Message::Assistant(AssistantMessage::empty()),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "read".into(),
                content: vec![crate::ai::types::ToolResultContent::text("x")],
                is_error: false,
                timestamp: 0,
            }),
            Message::Assistant(AssistantMessage::empty()),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "t2".into(),
                tool_name: "read".into(),
                content: vec![crate::ai::types::ToolResultContent::text("y")],
                is_error: false,
                timestamp: 0,
            }),
        ];
        let s = session_with(msgs.clone());
        assert_eq!(s.compaction_cut(), 0, "no safe user boundary → no cut");
        assert_eq!(s.messages.len(), msgs.len(), "nothing dropped");
    }

    #[test]
    fn apply_compaction_folds_summary_ahead_of_kept_user_text() {
        // Drop the first 2 messages; the summary must be folded into the (now
        // first) kept user message, ahead of that message's original text, and
        // the transcript must still start with a user turn.
        let msgs = vec![
            Message::User(UserMessage::text("old prompt")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("kept prompt")),
            Message::Assistant(AssistantMessage::empty()),
        ];
        let mut s = session_with(msgs);
        s.apply_compaction(2, Some("The user wanted to refactor parse().".into()));
        assert_eq!(s.messages.len(), 2, "dropped 2, kept 2");
        match &s.messages[0] {
            Message::User(u) => {
                let t = u.text_content();
                assert!(t.contains("Compaction summary"), "summary header present: {t}");
                assert!(
                    t.contains("The user wanted to refactor parse()."),
                    "summary body present: {t}"
                );
                assert!(t.contains("kept prompt"), "original user text preserved: {t}");
                // Summary must come BEFORE the original text so the model reads it first.
                assert!(t.find("Compaction summary").unwrap() < t.find("kept prompt").unwrap());
            }
            _ => panic!("must start with a user message"),
        }
    }

    #[test]
    fn apply_compaction_falls_back_to_note_without_summary() {
        // No summary (model unavailable/failed) → a plain "dropped" note, and the
        // transcript still starts with a user message.
        let msgs = vec![
            Message::User(UserMessage::text("old")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("kept")),
            Message::Assistant(AssistantMessage::empty()),
        ];
        let mut s = session_with(msgs);
        s.apply_compaction(2, None);
        assert_eq!(s.messages.len(), 2);
        assert!(matches!(s.messages.first(), Some(Message::User(_))));
        match &s.messages[0] {
            Message::User(u) => assert!(u.text_content().contains("dropped")),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn compact_async_falls_back_when_model_unreachable() {
        // Exercises the real async compact() path end-to-end: summarize() opens a
        // stream, collect_assistant_text drains it, and on failure apply_compaction
        // falls back. The model points at an unreachable host so the summary call
        // fails fast (connection refused) — this proves the orchestration and the
        // graceful fallback WITHOUT depending on (or spending) a real API call.
        let mut model = crate::ai::builtin_models()[0].clone();
        model.context_window = 1; // tiny → always wants to compact
        let mut session = AgentSession::new(
            std::path::PathBuf::from("."),
            "sys".into(),
            model,
            crate::ai::ThinkingLevel::Off,
            vec![],
            reqwest::Client::new(),
        );
        // Pin the compaction-tier model to an unreachable host so summarize()
        // fails fast and compaction falls back to the plain drop (rather than
        // hitting a real endpoint).
        let mut unreachable = crate::ai::builtin_models()[0].clone();
        unreachable.base_url = "http://127.0.0.1:1".into();
        session.compact_model = Some(unreachable);
        session.messages = vec![
            Message::User(UserMessage::text("first")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("second")),
            Message::Assistant(AssistantMessage::empty()),
            Message::User(UserMessage::text("third")),
            Message::Assistant(AssistantMessage::empty()),
        ];
        let dropped = session.compact().await;
        assert!(dropped >= 2, "should drop at least one exchange");
        assert!(matches!(session.messages.first(), Some(Message::User(_))));
        let text = match &session.messages[0] {
            Message::User(u) => u.text_content(),
            _ => unreachable!(),
        };
        assert!(
            text.contains("dropped"),
            "must fall back to the drop-note when the model is unreachable: {text}"
        );
    }
}
