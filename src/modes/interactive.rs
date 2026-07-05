//! Interactive REPL mode — `pi` (optionally with an initial prompt).

use std::io::Write;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use pixie_pi::ai::{self, ThinkingLevel};
use crate::modes::drive;
use crate::render::{blue, dim, green, magenta, red, yellow, EventRenderer};
use pixie_pi::session::AgentSession;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_FILE: &str = "history.txt";

enum SlashResult {
    Exit,
    Continue,
}

/// Run the interactive REPL. Returns the process exit code.
pub async fn run_interactive(
    mut session: AgentSession,
    initial: Option<ai::Message>,
    show_thinking: bool,
) -> Result<i32> {
    let mut rl = DefaultEditor::new()?;
    let hist_path = pixie_pi::config::agent_dir().join(HISTORY_FILE);
    let _ = rl.load_history(&hist_path);

    print_banner(&session);

    if let Some(msg) = initial {
        run_one(&mut session, msg, show_thinking).await;
    }

    let prompt = format!("{} ", dim("❯"));
    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(trimmed);
                if trimmed.starts_with('/') {
                    match handle_slash(&mut session, trimmed).await {
                        SlashResult::Exit => break,
                        SlashResult::Continue => {}
                    }
                } else {
                    // Plain prompt — send it to the model as a new user turn.
                    let msg = ai::Message::User(ai::UserMessage::text(trimmed));
                    run_one(&mut session, msg, show_thinking).await;
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ignore Ctrl-C at the prompt.
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("{}", red(&format!("input error: {e}")));
                break;
            }
        }
    }

    let _ = rl.save_history(&hist_path);
    Ok(0)
}

fn print_banner(session: &AgentSession) {
    let usage_pct = (session.context_usage() * 100.0).round() as u64;
    // Simplified banner - essential info only
    println!(
        "{} v{} — {} — {}%",
        magenta("pixie-pi"),
        VERSION,
        green(&session.model.id),
        usage_pct
    );
    println!("{}", dim(&format!("cwd: {}", session.cwd.display())));
    // Only show tools hint on first launch
    println!("{}", dim("Type /help for commands"));
    println!();
}

async fn run_one(session: &mut AgentSession, msg: ai::Message, show_thinking: bool) {
    let mut renderer = EventRenderer::new(show_thinking);
    drive(session, vec![msg], |ev| {
        renderer.handle(ev);
        matches!(ev, pixie_pi::agent::context::AgentEvent::AgentEnd { .. })
    })
    .await;
    // Only print cost/context summary when there's significant usage
    let cost = session.total_usage.cost.total;
    let pct = (session.context_usage() * 100.0).round() as u64;
    // Only show summary if cost > $0.01 or context > 50%
    if cost > 0.01 || pct > 50 {
        eprintln!(
            "{}",
            dim(&format!(
                "  ↳ {} out, {} in, ${:.4}, ctx {}%",
                session.total_usage.output,
                session.total_usage.input,
                cost,
                pct
            ))
        );
    }
    let _ = std::io::stderr().flush();
}

async fn handle_slash(session: &mut AgentSession, input: &str) -> SlashResult {
    let mut parts = input[1..].split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "exit" | "quit" | "q" => SlashResult::Exit,
        "help" | "h" | "?" => {
            println!("{}", blue("Available commands:"));
            let cmds = [
                ("/help", "show available commands"),
                ("/exit", "quit the session"),
                ("/clear", "clear conversation"),
                ("/model <id>", "switch model"),
                ("/thinking <lvl>", "set thinking level"),
                ("/compact", "compress old messages"),
                ("/context", "show token usage"),
            ];
            for (c, d) in cmds {
                println!("  {}  {}", yellow(c), dim(d));
            }
            SlashResult::Continue
        }
        "clear" => {
            session.messages.clear();
            println!("{}", green("Conversation cleared."));
            SlashResult::Continue
        }
        "model" => {
            let pattern = rest.join(" ");
            if pattern.is_empty() {
                println!("{}", dim(&format!("current: {}", session.model.id)));
                return SlashResult::Continue;
            }
            match ai::resolve_model(&ai::builtin_models(), &pattern) {
                Some(m) => {
                    println!("{} → {}", dim(&session.model.id), green(&m.id));
                    session.model = m;
                }
                None => {
                    eprintln!("{}", red("Unknown model"));
                }
            }
            SlashResult::Continue
        }
        "thinking" => {
            let level = rest.first().copied().unwrap_or("");
            match ThinkingLevel::parse(level) {
                Some(t) => {
                    session.thinking = t;
                    println!("{}", dim(&format!("thinking: {:?}", t)));
                }
                None => {
                    eprintln!("{}", red("Invalid thinking level (off/minimal/low/medium/high/xhigh)"));
                }
            }
            SlashResult::Continue
        }
        "compact" => {
            let dropped = session.compact().await;
            let _ = session.save();
            println!("{}", green(&format!("Compacted: {} messages", dropped)));
            SlashResult::Continue
        }
        "tools" => {
            let tools = session.tool_names().join(", ");
            println!("{} {}", dim("tools"), tools);
            SlashResult::Continue
        }
        "context" | "ctx" => {
            let pct = (session.context_usage() * 100.0).round() as u64;
            let estimated = session.estimated_tokens();
            println!("{}", dim(&format!("{}/{} tokens ({}%)", estimated, session.model.context_window, pct)));
            SlashResult::Continue
        }
        "cost" => {
            let u = &session.total_usage;
            println!("${:.6} ({} in, {} out)", u.cost.total, u.input, u.output);
            SlashResult::Continue
        }
        "system" => {
            let s = &session.system_prompt;
            let preview: String = s.chars().take(400).collect();
            println!("{}", preview);
            if s.chars().count() > 400 {
                println!("{}", dim("…"));
            }
            SlashResult::Continue
        }
        "" => SlashResult::Continue,
        other => {
            eprintln!("{}", red(&format!("Unknown command: /{} (try /help)", other)));
            SlashResult::Continue
        }
    }
}
