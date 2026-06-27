//! Anthropic Messages API provider — direct HTTP streaming with a hand-rolled
//! SSE decoder (no SDK dependency), mirroring `packages/ai/providers/anthropic.ts`.

use std::collections::HashMap;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::stream::{
    channel_stream, parse_streaming_json, AssistantMessageEvent, AssistantStream, SseDecoder,
};
use super::types::{
    AssistantMessage, ContentBlock, Message, Model, StopReason, ThinkingLevel, ToolSchema, Usage,
};

/// Options for a streaming request.
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    pub api_key: Option<String>,
    /// If set, sent as `Authorization: Bearer <token>` instead of `x-api-key`.
    pub auth_token: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f64>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budget_tokens: Option<usize>,
    /// Attach `cache_control: ephemeral` to the system prompt and the last
    /// user message to enable Anthropic prompt caching. Default: true.
    pub cache_control: bool,
    pub timeout_ms: Option<u64>,
}

impl SimpleStreamOptions {
    pub fn cache_control(mut self, on: bool) -> Self {
        self.cache_control = on;
        self
    }
}

/// The LLM-facing context: system prompt + transcript + tool schemas.
pub struct LlmContext<'a> {
    pub system_prompt: Option<&'a str>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Per content-block streaming scratch: the provider's block index maps to our
/// in-message index, the block kind, and any accumulated tool-call JSON.
#[derive(Default)]
struct BlockScratch {
    our_index: usize,
    kind: String,
    partial_json: String,
}

/// Stream a model response as a fine-grained event stream.
///
/// `stream_simple` maps a [`ThinkingLevel`] onto Anthropic's adaptive-effort or
/// budget-based thinking, then delegates to the raw streaming implementation.
/// The returned stream always terminates with a single `Done` or `Error`
/// event carrying the authoritative [`AssistantMessage`].
pub fn stream_simple(
    client: &reqwest::Client,
    model: &Model,
    context: &LlmContext<'_>,
    options: &SimpleStreamOptions,
    cancel: CancellationToken,
) -> AssistantStream {
    let body = build_request_body(model, context, options);
    let headers = build_headers(model, options);
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let model = model.clone();
    let timeout = options.timeout_ms;
    let cancel_inner = cancel.clone();
    let (tx, stream) = channel_stream(64);

    let client = client.clone();
    tokio::spawn(async move {
        let cancel = cancel_inner;
        let req = client
            .post(&url)
            .headers(headers)
            .json(&body);
        let req = if let Some(ms) = timeout {
            req.timeout(std::time::Duration::from_millis(ms))
        } else {
            req
        };

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                emit_terminal(&tx, StopReason::Aborted, "Request aborted").await;
                return;
            }
            res = req.send() => match res {
                Ok(r) => r,
                Err(e) => {
                    emit_terminal(&tx, StopReason::Error, &format!("Request failed: {e}")).await;
                    return;
                }
            },
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            emit_terminal(
                &tx,
                StopReason::Error,
                &format!("Anthropic API error ({}): {}", status, truncate(&text, 2000)),
            )
            .await;
            return;
        }

        let _ = tx.send(AssistantMessageEvent::Start).await;
        run_body(response, &model, cancel, tx).await;
    });

    stream
}

async fn run_body(
    response: reqwest::Response,
    model: &Model,
    cancel: CancellationToken,
    tx: tokio::sync::mpsc::Sender<AssistantMessageEvent>,
) {
    let mut output = AssistantMessage::empty();
    output.model = model.id.clone();
    output.provider = model.provider.clone();

    let mut decoder = SseDecoder::new();
    let mut blocks: HashMap<usize, BlockScratch> = HashMap::new();
    let mut saw_start = false;
    let mut saw_stop = false;

    let mut body = response.bytes_stream();
    let mut stream_error: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                emit_terminal(&tx, StopReason::Aborted, "Request aborted").await;
                return;
            }
            c = body.next() => c,
        };
        match chunk {
            Some(Ok(bytes)) => {
                for ev in decoder.feed(&bytes) {
                    handle_sse(
                        ev, &mut output, &mut blocks, &mut saw_start, &mut saw_stop, &tx,
                    )
                    .await;
                }
            }
            Some(Err(e)) => {
                stream_error = Some(format!("stream read error: {e}"));
                break;
            }
            None => break,
        }
    }

    for ev in decoder.finish() {
        handle_sse(
            ev, &mut output, &mut blocks, &mut saw_start, &mut saw_stop, &tx,
        )
        .await;
    }

    if let Some(msg) = stream_error {
        if saw_start && !saw_stop {
            warn!(error = %msg, "anthropic stream ended prematurely");
        }
    }

    if saw_start && !saw_stop {
        finalize_message(&mut output, model, StopReason::Error);
        output.error_message = Some("Anthropic stream ended before message_stop".into());
        let _ = tx
            .send(AssistantMessageEvent::Error {
                reason: StopReason::Error,
                message: output,
            })
            .await;
        return;
    }

    let reason = if cancel.is_cancelled() {
        StopReason::Aborted
    } else if output.stop_reason == StopReason::default() {
        StopReason::Stop
    } else {
        output.stop_reason
    };
    output.stop_reason = reason;
    finalize_message(&mut output, model, reason);
    let _ = tx
        .send(AssistantMessageEvent::Done {
            reason,
            message: output,
        })
        .await;
}

async fn handle_sse(
    sse: super::stream::ServerSentEvent,
    output: &mut AssistantMessage,
    blocks: &mut HashMap<usize, BlockScratch>,
    saw_start: &mut bool,
    saw_stop: &mut bool,
    tx: &tokio::sync::mpsc::Sender<AssistantMessageEvent>,
) {
    if sse.event.as_deref() == Some("error") {
        // Surface as an error message; the terminal event is emitted by caller.
        output.error_message = Some(sse.data.clone());
        return;
    }
    let parsed: Value = match serde_json::from_str(&sse.data) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ty = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "message_start" => {
            *saw_start = true;
            if let Some(msg) = parsed.get("message") {
                if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                    output.response_id = Some(id.to_string());
                }
                if let Some(u) = msg.get("usage") {
                    apply_usage(&mut output.usage, u);
                }
            }
        }
        "content_block_start" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let block = parsed.get("content_block").cloned().unwrap_or(Value::Null);
            let kind = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text")
                .to_string();
            let our_index = output.content.len();
            match kind.as_str() {
                "text" => {
                    output.content.push(ContentBlock::Text { text: String::new() });
                    let _ = tx
                        .send(AssistantMessageEvent::TextStart { content_index: our_index })
                        .await;
                }
                "thinking" => {
                    output.content.push(ContentBlock::Thinking {
                        thinking: String::new(),
                        thinking_signature: String::new(),
                        redacted: false,
                    });
                    let _ = tx
                        .send(AssistantMessageEvent::ThinkingStart { content_index: our_index })
                        .await;
                }
                "redacted_thinking" => {
                    let data = block
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    output.content.push(ContentBlock::Thinking {
                        thinking: "[Reasoning redacted]".into(),
                        thinking_signature: data,
                        redacted: true,
                    });
                    let _ = tx
                        .send(AssistantMessageEvent::ThinkingStart { content_index: our_index })
                        .await;
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    output.content.push(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments: Value::Object(Default::default()),
                    });
                    let _ = tx
                        .send(AssistantMessageEvent::ToolCallStart { content_index: our_index })
                        .await;
                }
                _ => return,
            }
            blocks.insert(
                index,
                BlockScratch {
                    our_index,
                    kind,
                    partial_json: String::new(),
                },
            );
        }
        "content_block_delta" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = parsed.get("delta").cloned().unwrap_or(Value::Null);
            let dtype = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let Some(scratch) = blocks.get_mut(&index) else {
                return;
            };
            let our_index = scratch.our_index;
            let Some(block) = output.content.get_mut(our_index) else {
                return;
            };
            match (dtype, block) {
                ("text_delta", ContentBlock::Text { text }) => {
                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                        let _ = tx
                            .send(AssistantMessageEvent::TextDelta {
                                content_index: our_index,
                                delta: t.to_string(),
                            })
                            .await;
                    }
                }
                ("thinking_delta", ContentBlock::Thinking { thinking, .. }) => {
                    if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                        thinking.push_str(t);
                        let _ = tx
                            .send(AssistantMessageEvent::ThinkingDelta {
                                content_index: our_index,
                                delta: t.to_string(),
                            })
                            .await;
                    }
                }
                ("signature_delta", ContentBlock::Thinking { thinking_signature, .. }) => {
                    if let Some(s) = delta.get("signature").and_then(|v| v.as_str()) {
                        thinking_signature.push_str(s);
                    }
                }
                ("input_json_delta", ContentBlock::ToolCall { arguments, .. }) => {
                    if let Some(p) = delta.get("partial_json").and_then(|v| v.as_str()) {
                        scratch.partial_json.push_str(p);
                        let _ = tx
                            .send(AssistantMessageEvent::ToolCallDelta {
                                content_index: our_index,
                                delta: p.to_string(),
                            })
                            .await;
                        *arguments = parse_streaming_json(&scratch.partial_json);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(scratch) = blocks.remove(&index) {
                let our_index = scratch.our_index;
                if let Some(ContentBlock::ToolCall { arguments, .. }) = output.content.get_mut(our_index)
                {
                    *arguments = parse_streaming_json(&scratch.partial_json);
                }
                let event = match scratch.kind.as_str() {
                    "text" => AssistantMessageEvent::TextEnd { content_index: our_index },
                    "thinking" | "redacted_thinking" => {
                        AssistantMessageEvent::ThinkingEnd { content_index: our_index }
                    }
                    "tool_use" => AssistantMessageEvent::ToolCallEnd { content_index: our_index },
                    _ => return,
                };
                let _ = tx.send(event).await;
            }
        }
        "message_delta" => {
            if let Some(reason) = parsed
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
            {
                output.stop_reason = map_stop_reason(reason);
            }
            if let Some(u) = parsed.get("usage") {
                apply_usage(&mut output.usage, u);
            }
        }
        "message_stop" => {
            *saw_stop = true;
        }
        _ => {}
    }
}

fn finalize_message(output: &mut AssistantMessage, model: &Model, _reason: StopReason) {
    output.usage.recompute_total();
    let cost = model.cost_for(&output.usage);
    output.usage.cost = cost;
}

fn apply_usage(usage: &mut Usage, value: &Value) {
    if let Some(v) = value.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.input = v;
    }
    if let Some(v) = value.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.output = v;
    }
    if let Some(v) = value.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_read = v;
    }
    if let Some(v) = value.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_write = v;
    }
    usage.recompute_total();
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "refusal" | "sensitive" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

async fn emit_terminal(
    tx: &tokio::sync::mpsc::Sender<AssistantMessageEvent>,
    reason: StopReason,
    message: &str,
) {
    let mut msg = AssistantMessage::empty();
    msg.stop_reason = reason;
    msg.error_message = Some(message.to_string());
    let _ = tx
        .send(AssistantMessageEvent::Error {
            reason,
            message: msg,
        })
        .await;
}

fn truncate(s: &str, max: usize) -> String {
    // Operate on chars, not bytes, so a multi-byte character at the cut point
    // (common in CJK / emoji error bodies) doesn't panic on `&s[..max]`.
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn build_headers(model: &Model, options: &SimpleStreamOptions) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        HeaderValue::from_static("true"),
    );
    if let Some(token) = &options.auth_token {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert("authorization", v);
        }
    } else if let Some(key) = &options.api_key {
        if let Ok(v) = HeaderValue::from_str(key) {
            headers.insert("x-api-key", v);
        }
    }
    // Per-model custom headers (e.g. proxy gateway auth) are not modeled here.
    let _ = model;
    headers
}

fn cache_ephemeral() -> Value {
    json!({ "type": "ephemeral" })
}

/// Translate a [`ThinkingLevel`] to an Anthropic effort string for adaptive
/// models. Returns `None` when thinking is off.
fn level_to_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("xhigh"),
    }
}

fn level_to_budget(level: ThinkingLevel) -> usize {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 4_096,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High => 16_000,
        ThinkingLevel::Xhigh => 24_000,
    }
}

fn build_request_body(
    model: &Model,
    context: &LlmContext<'_>,
    options: &SimpleStreamOptions,
) -> Value {
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let cache = if options.cache_control {
        Some(cache_ephemeral())
    } else {
        None
    };

    // System prompt (with optional cache_control).
    let system: Vec<Value> = match context.system_prompt {
        Some(prompt) if !prompt.trim().is_empty() => {
            let mut block = json!({ "type": "text", "text": prompt });
            if let Some(cc) = &cache {
                block["cache_control"] = cc.clone();
            }
            vec![block]
        }
        _ => Vec::new(),
    };

    let messages = convert_messages(context.messages, cache.as_ref());

    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }

    if model.reasoning {
        match options.reasoning {
            None => {
                body["thinking"] = json!({ "type": "disabled" });
            }
            Some(ThinkingLevel::Off) => {
                body["thinking"] = json!({ "type": "disabled" });
            }
            Some(level) if model.force_adaptive_thinking => {
                let mut thinking = json!({ "type": "adaptive", "display": "summarized" });
                if let Some(effort) = level_to_effort(level) {
                    thinking["effort"] = json!(effort);
                    body["output_config"] = json!({ "effort": effort });
                }
                body["thinking"] = thinking;
            }
            Some(level) => {
                let budget = options
                    .thinking_budget_tokens
                    .unwrap_or_else(|| level_to_budget(level));
                let budget = budget.min(max_tokens.saturating_sub(1)).max(1024);
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                    "display": "summarized",
                });
            }
        }
    } else if let Some(temp) = options.temperature {
        if model.supports_temperature {
            body["temperature"] = json!(temp);
        }
    } else if model.supports_temperature {
        // No explicit temperature and not thinking: leave default.
    }

    if !context.tools.is_empty() {
        body["tools"] = Value::Array(convert_tools(context.tools, cache.as_ref()));
    }

    body
}

/// Convert the transcript into Anthropic `MessageParam[]`, grouping
/// consecutive tool results into a single `user` message (required by the
/// API) and attaching cache_control to the last user message block.
fn convert_messages(messages: &[Message], cache: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::User(u) => {
                let has_images = u.content.iter().any(|b| matches!(b, super::types::UserBlock::Image { .. }));
                if !has_images {
                    let text = u.text_content();
                    if text.trim().is_empty() {
                        i += 1;
                        continue;
                    }
                    out.push(json!({ "role": "user", "content": text }));
                } else {
                    let blocks: Vec<Value> = u
                        .content
                        .iter()
                        .map(|b| match b {
                            super::types::UserBlock::Text { text } => {
                                json!({ "type": "text", "text": text })
                            }
                            super::types::UserBlock::Image { data, mime_type } => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": mime_type,
                                    "data": data,
                                }
                            }),
                        })
                        .filter(|b| !(b.get("type").and_then(|v| v.as_str()) == Some("text")
                            && b.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().is_empty()))
                        .collect();
                    if blocks.is_empty() {
                        i += 1;
                        continue;
                    }
                    out.push(json!({ "role": "user", "content": blocks }));
                }
                i += 1;
            }
            Message::Assistant(a) => {
                let mut blocks: Vec<Value> = Vec::new();
                for c in &a.content {
                    match c {
                        ContentBlock::Text { text } => {
                            if !text.trim().is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted {
                                blocks.push(json!({ "type": "redacted_thinking", "data": thinking_signature }));
                            } else if !thinking.trim().is_empty() {
                                if thinking_signature.is_empty() {
                                    blocks.push(json!({ "type": "text", "text": thinking }));
                                } else {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": thinking,
                                        "signature": thinking_signature,
                                    }));
                                }
                            }
                        }
                        ContentBlock::ToolCall { id, name, arguments } => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                // Collect all consecutive tool results into one user message.
                let mut results: Vec<Value> = Vec::new();
                while i < messages.len() {
                    if let Message::ToolResult(tr) = &messages[i] {
                        let content = convert_result_content(&tr.content);
                        results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tr.tool_call_id,
                            "content": content,
                            "is_error": tr.is_error,
                        }));
                        i += 1;
                    } else {
                        break;
                    }
                }
                if !results.is_empty() {
                    out.push(json!({ "role": "user", "content": results }));
                }
            }
        }
    }

    // Attach cache_control to the last block of the last user message.
    if let Some(cc) = cache {
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some("user") {
                if let Some(content) = last.get_mut("content") {
                    if let Some(arr) = content.as_array_mut() {
                        if let Some(last_block) = arr.last_mut() {
                            last_block["cache_control"] = cc.clone();
                        }
                    } else if let Some(s) = content.as_str() {
                        let block = json!({ "type": "text", "text": s, "cache_control": cc });
                        *content = Value::Array(vec![block]);
                    }
                }
            }
        }
    }

    out
}

fn convert_result_content(content: &[super::types::ToolResultContent]) -> Value {
    let has_image = content
        .iter()
        .any(|c| matches!(c, super::types::ToolResultContent::Image { .. }));
    if !has_image {
        let text = content
            .iter()
            .filter_map(|c| match c {
                super::types::ToolResultContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Value::String(text)
    } else {
        Value::Array(
            content
                .iter()
                .map(|c| match c {
                    super::types::ToolResultContent::Text { text } => {
                        json!({ "type": "text", "text": text })
                    }
                    super::types::ToolResultContent::Image { data, mime_type } => json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": mime_type, "data": data },
                    }),
                })
                .collect(),
        )
    }
}

fn convert_tools(tools: &[ToolSchema], cache: Option<&Value>) -> Vec<Value> {
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let schema = &t.input_schema;
            let properties = schema
                .get("properties")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let required = schema
                .get("required")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let mut entry = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                },
            });
            if i == tools.len() - 1 {
                if let Some(cc) = cache {
                    entry["cache_control"] = cc.clone();
                }
            }
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_is_char_safe_on_multibyte() {
        // A byte-based `&s[..2000]` cut would land mid-character and panic.
        let s = "中".repeat(3000); // 3 bytes/char
        let t = truncate(&s, 2000);
        assert!(t.chars().count() <= 2001); // 2000 chars + ellipsis
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_short_input_unchanged() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("ab", 2), "ab");
    }

    #[test]
    fn truncate_cuts_at_char_boundary() {
        let t = truncate("a中b", 2);
        assert_eq!(t, "a中…");
    }
}
