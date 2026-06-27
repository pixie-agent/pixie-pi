//! The agent loop (`packages/agent/agent-loop.ts`): stream an assistant response,
//! execute any tool calls, feed results back, and repeat until the model stops.

use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::agent::context::{AgentContext, AgentEvent};
use crate::agent::tool::{
    validate_required, AgentTool, ExecutionMode, ToolCallContext, ToolGate, ToolResult,
};
use crate::ai::anthropic::{stream_simple, LlmContext, SimpleStreamOptions};
use crate::ai::stream::AssistantMessageEvent;
use crate::ai::types::{
    AssistantMessage, ContentBlock, Message, Model, ToolResultMessage,
};

/// Configuration for the agent loop.
pub struct AgentLoopConfig {
    pub client: reqwest::Client,
    pub model: Model,
    pub options: SimpleStreamOptions,
    pub tool_execution: ExecutionMode,
    pub gate: Option<Arc<dyn ToolGate>>,
}

/// A boxed async stream of agent events.
pub type AgentEventStream = ReceiverStream<AgentEvent>;

/// Start an agent loop as a background task and return its event stream. The
/// loop appends `prompts` to a copy of the context, runs to completion, and
/// terminates with a single [`AgentEvent::AgentEnd`] carrying the final
/// transcript. Each run is independent (the caller owns the live transcript).
pub fn agent_loop(
    ctx: AgentContext,
    config: AgentLoopConfig,
    prompts: Vec<Message>,
    cancel: CancellationToken,
) -> AgentEventStream {
    let (tx, rx) = mpsc::channel(128);
    let tx2 = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_loop(ctx, config, prompts, cancel, tx).await {
            let _ = tx2.send(AgentEvent::Error(e.to_string())).await;
        }
    });
    ReceiverStream::new(rx)
}

async fn run_loop(
    mut ctx: AgentContext,
    config: AgentLoopConfig,
    prompts: Vec<Message>,
    cancel: CancellationToken,
    tx: mpsc::Sender<AgentEvent>,
) -> anyhow::Result<()> {
    let _ = tx.send(AgentEvent::AgentStart).await;

    // Inject the prompt messages.
    for prompt in &prompts {
        ctx.messages.push(prompt.clone());
        let _ = tx.send(AgentEvent::MessageStart(prompt.clone())).await;
        let _ = tx.send(AgentEvent::MessageEnd(prompt.clone())).await;
    }

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let _ = tx.send(AgentEvent::TurnStart).await;

        let assistant = match stream_assistant(&mut ctx, &config, cancel.clone(), &tx).await {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                break;
            }
        };

        let _ = tx.send(AgentEvent::Usage(assistant.usage.clone())).await;

        let tool_calls: Vec<(String, String, Value)> = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, name, arguments } => {
                    Some((id.clone(), name.clone(), arguments.clone()))
                }
                _ => None,
            })
            .collect();

        let tool_results: Vec<ToolResultMessage> = if tool_calls.is_empty() {
            Vec::new()
        } else {
            execute_tool_calls(&ctx, &tool_calls, &config, cancel.clone(), &tx).await
        };

        for result in &tool_results {
            let msg = Message::ToolResult(result.clone());
            ctx.messages.push(msg.clone());
            let _ = tx.send(AgentEvent::MessageStart(msg.clone())).await;
            let _ = tx.send(AgentEvent::MessageEnd(msg)).await;
        }

        let _ = tx
            .send(AgentEvent::TurnEnd {
                message: assistant,
                tool_results,
            })
            .await;

        // Stop when the assistant produced no tool calls (it's done), or the
        // run was aborted. Otherwise the tool results are now in context and
        // we loop to let the model respond again.
        if tool_calls.is_empty() || cancel.is_cancelled() {
            break;
        }
    }

    let _ = tx.send(AgentEvent::AgentEnd { messages: ctx.messages }).await;
    Ok(())
}

/// Stream a single assistant response, mutating the context's transcript.
async fn stream_assistant(
    ctx: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    tx: &mpsc::Sender<AgentEvent>,
) -> anyhow::Result<AssistantMessage> {
    let tools_schema: Vec<_> = ctx.tools.iter().map(|t| t.schema()).collect();
    let llm = LlmContext {
        system_prompt: Some(&ctx.system_prompt),
        messages: &ctx.messages,
        tools: &tools_schema,
    };
    let stream = stream_simple(&config.client, &config.model, &llm, &config.options, cancel);

    let mut partial = AssistantMessage::empty();
    partial.model = config.model.id.clone();
    partial.provider = config.model.provider.clone();
    ctx.messages.push(Message::Assistant(partial.clone()));
    let _ = tx
        .send(AgentEvent::MessageStart(Message::Assistant(partial.clone())))
        .await;

    let mut final_msg: Option<AssistantMessage> = None;
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        match &event {
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { message, .. } => {
                final_msg = Some(message.clone());
                apply_event(&mut partial, &event);
                let _ = tx.send(AgentEvent::MessageUpdate { event }).await;
            }
            _ => {
                apply_event(&mut partial, &event);
                let _ = tx.send(AgentEvent::MessageUpdate { event }).await;
            }
        }
    }

    let final_msg = final_msg.unwrap_or(partial);
    // Replace the placeholder assistant message with the authoritative one.
    if let Some(Message::Assistant(slot)) = ctx.messages.last_mut() {
        *slot = final_msg.clone();
    }
    let _ = tx
        .send(AgentEvent::MessageEnd(Message::Assistant(final_msg.clone())))
        .await;
    Ok(final_msg)
}

/// Reconstruct a partial assistant message from fine-grained events (for live
/// rendering). The authoritative message arrives via `Done`/`Error`.
fn apply_event(msg: &mut AssistantMessage, event: &AssistantMessageEvent) {
    match event {
        AssistantMessageEvent::Start => {}
        AssistantMessageEvent::TextStart { content_index } => {
            ensure_block(msg, *content_index, || ContentBlock::Text { text: String::new() });
        }
        AssistantMessageEvent::TextDelta { content_index, delta } => {
            if let Some(ContentBlock::Text { text }) = msg.content.get_mut(*content_index) {
                text.push_str(delta);
            }
        }
        AssistantMessageEvent::TextEnd { .. } => {}
        AssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_block(msg, *content_index, || ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: String::new(),
                redacted: false,
            });
        }
        AssistantMessageEvent::ThinkingDelta { content_index, delta } => {
            if let Some(ContentBlock::Thinking { thinking, .. }) = msg.content.get_mut(*content_index)
            {
                thinking.push_str(delta);
            }
        }
        AssistantMessageEvent::ThinkingEnd { .. } => {}
        AssistantMessageEvent::ToolCallStart { content_index } => {
            ensure_block(msg, *content_index, || ContentBlock::ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: Value::Object(Default::default()),
            });
        }
        AssistantMessageEvent::ToolCallDelta { .. } => {
            // Live tool-argument deltas are parsed by the provider; the final
            // arguments arrive with the authoritative message.
        }
        AssistantMessageEvent::ToolCallEnd { .. } => {}
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { message, .. } => {
            *msg = message.clone();
        }
    }
}

fn ensure_block(msg: &mut AssistantMessage, index: usize, make: impl Fn() -> ContentBlock) {
    while msg.content.len() <= index {
        msg.content.push(make());
    }
}

/// Execute all tool calls from one assistant message and return the result
/// messages (in tool-call order). Respects per-tool / config execution mode.
async fn execute_tool_calls(
    ctx: &AgentContext,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    tx: &mpsc::Sender<AgentEvent>,
) -> Vec<ToolResultMessage> {
    let force_sequential = config.tool_execution == ExecutionMode::Sequential
        || tool_calls.iter().any(|(_id, name, _)| {
            id_is_sequential(ctx, name)
        });

    if force_sequential {
        sequential_execute(ctx, tool_calls, config, cancel, tx).await
    } else {
        parallel_execute(ctx, tool_calls, config, cancel, tx).await
    }
}

fn id_is_sequential(ctx: &AgentContext, name: &str) -> bool {
    ctx.find_tool(name)
        .map(|t| t.execution_mode() == ExecutionMode::Sequential)
        .unwrap_or(false)
}

struct Prepared {
    tool: Arc<dyn AgentTool>,
    args: Value,
}

enum PreparedOutcome {
    Run(Prepared),
    Immediate(ToolResult, bool /*is_error*/),
}

async fn prepare_one(
    ctx: &AgentContext,
    tool_call: &(String, String, Value),
    config: &AgentLoopConfig,
    tx: &mpsc::Sender<AgentEvent>,
) -> PreparedOutcome {
    let (id, name, args) = tool_call;
    let _ = tx
        .send(AgentEvent::ToolExecutionStart {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            args: args.clone(),
        })
        .await;

    let Some(tool) = ctx.find_tool(name).cloned() else {
        return PreparedOutcome::Immediate(
            ToolResult::error(format!("Tool {name} not found")),
            true,
        );
    };

    let prepared_args = tool.prepare_arguments(args.clone());
    if let Err(e) = validate_required(&tool.input_schema(), &prepared_args) {
        return PreparedOutcome::Immediate(ToolResult::error(e), true);
    }

    if let Some(gate) = &config.gate {
        let res = gate
            .before(ToolCallContext {
                tool_call_id: id,
                tool_name: name,
                args: &prepared_args,
            })
            .await;
        if res.block {
            let reason = res
                .reason
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            return PreparedOutcome::Immediate(ToolResult::error(reason), true);
        }
    }

    PreparedOutcome::Run(Prepared {
        tool,
        args: prepared_args,
    })
}

async fn run_prepared(
    prepared: Prepared,
    tool_call: &(String, String, Value),
    cancel: CancellationToken,
    tx: &mpsc::Sender<AgentEvent>,
) -> (ToolResult, bool) {
    let (id, name, _args) = tool_call;
    let child = cancel.child_token();
    let result = prepared.tool.execute(prepared.args, child).await;
    let (result, is_error) = match result {
        Ok(r) => (r, false),
        Err(e) => (ToolResult::error(e.to_string()), true),
    };
    let _ = tx
        .send(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            is_error,
        })
        .await;
    (result, is_error)
}

fn build_result_message(
    tool_call: &(String, String, Value),
    result: ToolResult,
    is_error: bool,
) -> ToolResultMessage {
    let (id, name, _) = tool_call;
    ToolResultMessage {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        content: result.content,
        is_error,
        timestamp: crate::ai::now_ms(),
    }
}

async fn sequential_execute(
    ctx: &AgentContext,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    tx: &mpsc::Sender<AgentEvent>,
) -> Vec<ToolResultMessage> {
    let mut messages = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        if cancel.is_cancelled() {
            break;
        }
        let outcome = prepare_one(ctx, tc, config, tx).await;
        let (result, is_error) = match outcome {
            PreparedOutcome::Run(prepared) => run_prepared(prepared, tc, cancel.clone(), tx).await,
            PreparedOutcome::Immediate(result, is_error) => {
                let (id, name, _) = tc;
                let _ = tx
                    .send(AgentEvent::ToolExecutionEnd {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        is_error,
                    })
                    .await;
                (result, is_error)
            }
        };
        messages.push(build_result_message(tc, result, is_error));
    }
    messages
}

async fn parallel_execute(
    ctx: &AgentContext,
    tool_calls: &[(String, String, Value)],
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    tx: &mpsc::Sender<AgentEvent>,
) -> Vec<ToolResultMessage> {
    // Prepare sequentially (emits start events in order), then run concurrently.
    let mut prepared: Vec<PreparedOutcome> = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        if cancel.is_cancelled() {
            break;
        }
        prepared.push(prepare_one(ctx, tc, config, tx).await);
    }

    let futures: Vec<_> = prepared
        .into_iter()
        .enumerate()
        .map(|(i, outcome)| {
            let tx = tx.clone();
            let cancel = cancel.clone();
            let tc = &tool_calls[i];
            async move {
                match outcome {
                    PreparedOutcome::Run(p) => run_prepared(p, tc, cancel, &tx).await,
                    PreparedOutcome::Immediate(result, is_error) => {
                        let (id, name, _) = tc;
                        let _ = tx
                            .send(AgentEvent::ToolExecutionEnd {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                is_error,
                            })
                            .await;
                        (result, is_error)
                    }
                }
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;
    results
        .into_iter()
        .enumerate()
        .map(|(i, (result, is_error))| build_result_message(&tool_calls[i], result, is_error))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::ContentBlock;

    #[test]
    fn apply_event_assembles_text() {
        let mut msg = AssistantMessage::empty();
        apply_event(&mut msg, &AssistantMessageEvent::TextStart { content_index: 0 });
        apply_event(
            &mut msg,
            &AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hello ".into(),
            },
        );
        apply_event(
            &mut msg,
            &AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "world".into(),
            },
        );
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected text block"),
        }
    }
}
