//! Streaming primitives: the SSE decoder, a tolerant streaming-JSON parser,
//! the fine-grained [`AssistantMessageEvent`] protocol, and the
//! [`AssistantStream`] type that providers return.

use std::pin::Pin;

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::types::{AssistantMessage, StopReason};

/// A raw Server-Sent Event decoded from the HTTP response body.
#[derive(Debug, Clone)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental line-buffered SSE decoder (mirrors pi's `iterateSseMessages`).
///
/// Feed it raw bytes via [`SseDecoder::feed`] and call [`SseDecoder::finish`]
/// when the stream ends. An SSE event is delimited by a blank line.
#[derive(Default)]
pub struct SseDecoder {
    event: Option<String>,
    data: Vec<String>,
    buffer: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return any complete SSE events.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ServerSentEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some((line, rest)) = consume_line(&self.buffer) {
            self.buffer = rest;
            if let Some(ev) = self.decode_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush any trailing buffered data. Call once the body stream ends.
    pub fn finish(&mut self) -> Vec<ServerSentEvent> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(ev) = self.decode_line(&line) {
                out.push(ev);
            }
        }
        if let Some(ev) = self.flush_event() {
            out.push(ev);
        }
        out
    }

    fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush_event();
        }
        // Comment line.
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(idx) => {
                let value = &line[idx + 1..];
                let value = value.strip_prefix(' ').unwrap_or(value);
                (&line[..idx], value)
            }
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    fn flush_event(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = ServerSentEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        Some(event)
    }
}

/// Split off the first line of `text`, handling both `\n` and `\r\n`.
fn consume_line(text: &str) -> Option<(String, String)> {
    let bytes = text.as_bytes();
    let cr = text.find('\r');
    let lf = text.find('\n');
    let idx = match (cr, lf) {
        (None, None) => return None,
        (None, Some(l)) => l,
        (Some(c), None) => c,
        (Some(c), Some(l)) => c.min(l),
    };
    let mut next = idx + 1;
    if bytes[idx] == b'\r' && bytes.get(next) == Some(&b'\n') {
        next += 1;
    }
    Some((text[..idx].to_string(), text[next..].to_string()))
}

/// Parse possibly-truncated streaming JSON (e.g. tool-call `input_json_delta`
/// fragments). Mirrors pi's `parseStreamingJson`: returns the best-effort
/// parsed value, repairing truncated input so the agent always gets usable
/// arguments to execute.
pub fn parse_streaming_json(input: &str) -> Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v;
    }
    match repair_json(trimmed) {
        Some(v) => v,
        None => Value::Object(Default::default()),
    }
}

/// Close any open string and unmatched brackets/braces so truncated JSON can
/// still be parsed.
fn repair_json(input: &str) -> Option<Value> {
    let bytes = input.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for &b in bytes {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => stack.push(b),
            b'}' => {
                if stack.last() == Some(&b'{') {
                    stack.pop();
                }
            }
            b']'
                if stack.last() == Some(&b'[') => {
                    stack.pop();
                }
            _ => {}
        }
    }
    let mut repaired = input.to_string();
    if in_string {
        repaired.push('"');
    }
    // A truncation can land right after a structural comma — streaming
    // `input_json_delta` chunks frequently split at exactly these token
    // boundaries, e.g. `{"path":"foo",`. The open string is already closed
    // above, so any comma now sitting at the tail is a *dangling structural*
    // comma, not one inside a value. Strip it (char-wise via `pop`) before the
    // closers below, otherwise the closed JSON keeps `,...}` and the whole
    // value is dropped to `{}` — silently losing every key already received.
    while repaired.ends_with(',') {
        repaired.pop();
    }
    while let Some(&open) = stack.last() {
        repaired.push(match open {
            b'{' => '}',
            b'[' => ']',
            _ => break,
        });
        stack.pop();
    }
    serde_json::from_str(&repaired).ok()
}

// ---------------------------------------------------------------------------
// Assistant message events
// ---------------------------------------------------------------------------

/// Fine-grained events emitted while streaming a model response. Mirrors
/// pi's `AssistantMessageEvent`.
///
/// Unlike the TS version we deliberately do **not** clone the full partial
/// message into every delta event (that would be O(n²) for long outputs).
/// The loop rebuilds the partial from start/delta/end events and replaces it
/// with the authoritative message carried by [`Self::Done`] / [`Self::Error`].
#[derive(Debug, Clone, serde::Serialize)]
pub enum AssistantMessageEvent {
    Start,
    TextStart { content_index: usize },
    TextDelta { content_index: usize, delta: String },
    TextEnd { content_index: usize },
    ThinkingStart { content_index: usize },
    ThinkingDelta { content_index: usize, delta: String },
    ThinkingEnd { content_index: usize },
    ToolCallStart { content_index: usize },
    ToolCallDelta { content_index: usize, delta: String },
    ToolCallEnd { content_index: usize },
    Done { reason: StopReason, message: AssistantMessage },
    Error { reason: StopReason, message: AssistantMessage },
}

/// A boxed, pinned async stream of assistant events.
pub type AssistantStream =
    Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + Unpin>>;

/// Build an [`AssistantStream`] from a channel pair. The provider pushes
/// events through the sender from a spawned task.
pub fn channel_stream(buffer: usize) -> (mpsc::Sender<AssistantMessageEvent>, AssistantStream) {
    let (tx, rx) = mpsc::channel(buffer);
    let stream: AssistantStream = Box::pin(ReceiverStream::new(rx));
    (tx, stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decodes_message_events() {
        let raw = b"event: message_start\n\
                    data: {\"type\":\"message_start\"}\n\
                    \n\
                    event: content_block_delta\n\
                    data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
                    \n\
                    event: message_stop\n\
                    data: {\"type\":\"message_stop\"}\n\n";
        let mut dec = SseDecoder::new();
        let events = dec.feed(raw);
        let events: Vec<_> = events.into_iter().chain(dec.finish()).collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[1].event.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn parse_streaming_json_repairs_truncated_input() {
        let v = parse_streaming_json(r#"{"path":"foo.txt"#);
        assert_eq!(v["path"], "foo.txt");

        let v = parse_streaming_json(r#"{"edits":[{"oldText":"a","newText":"b"}"#);
        assert_eq!(v["edits"][0]["oldText"], "a");

        // Already-complete JSON passes through unchanged.
        let v = parse_streaming_json(r#"{"command":"ls"}"#);
        assert_eq!(v["command"], "ls");

        // Empty input yields an empty object.
        assert!(parse_streaming_json("").is_object());
    }

    #[test]
    fn parse_streaming_json_repairs_trailing_comma() {
        // Truncation at a comma token boundary must keep the already-received
        // key, not drop the whole object to {}.
        let v = parse_streaming_json(r#"{"path":"foo","#);
        assert_eq!(v["path"], "foo");

        // A comma inside an *unclosed* string value is preserved: the string is
        // closed first, so only the dangling structural comma is stripped.
        let v = parse_streaming_json(r#"{"a":"x,y","#);
        assert_eq!(v["a"], "x,y");

        // Nested trailing comma inside a truncated array.
        let v = parse_streaming_json(r#"{"items":[1,2,"#);
        assert_eq!(v["items"][0], 1);
        assert_eq!(v["items"][1], 2);
    }

    #[test]
    fn consume_line_handles_crlf() {
        let (line, rest) = consume_line("abc\r\ndef").unwrap();
        assert_eq!(line, "abc");
        assert_eq!(rest, "def");
    }
}
