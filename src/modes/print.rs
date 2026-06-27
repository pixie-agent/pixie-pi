//! Command-line (one-shot) mode — `pi -p "prompt"` or `pi "prompt"` piped.
//! Streams the response and exits.

use anyhow::Result;

use crate::cli::OutputMode;
use crate::modes::drive;
use crate::render::EventRenderer;
use crate::session::AgentSession;

/// Run a single prompt to completion and return the process exit code.
pub async fn run_print(
    session: &mut AgentSession,
    prompt: Option<crate::ai::Message>,
    output: OutputMode,
    show_thinking: bool,
) -> Result<i32> {
    let prompts: Vec<crate::ai::Message> = prompt.into_iter().collect();

    let outcome = match output {
        OutputMode::Text => {
            let mut renderer = EventRenderer::new(show_thinking);
            drive(session, prompts, |ev| {
                renderer.handle(ev);
                matches!(ev, crate::agent::context::AgentEvent::AgentEnd { .. })
            })
            .await
        }
        OutputMode::Json => {
            drive(session, prompts, |ev| {
                if let Ok(line) = serde_json::to_string(ev) {
                    println!("{line}");
                }
                matches!(ev, crate::agent::context::AgentEvent::AgentEnd { .. })
            })
            .await
        }
    };

    Ok(if outcome.had_error { 1 } else { 0 })
}
