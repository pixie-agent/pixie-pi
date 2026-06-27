//! pixie-pi — an AI coding agent.
//!
//! Two usage forms:
//! - **command-line**: `pi -p "prompt"` (or `pi "prompt"` piped / non-TTY) —
//!   one-shot, streams the answer, exits.
//! - **interactive**: `pi` (with a TTY) — a conversational REPL with slash
//!   commands, streaming responses, and tool-call display.

// This binary also exposes a reusable library surface (the `ai`, `agent`,
// `tools`, and `session` modules). Public items not consumed by the binary
// itself form that API, so we don't treat them as dead code.
#![allow(dead_code)]

mod ai;
mod agent;
mod app;
mod cli;
mod config;
mod modes;
mod prompt;
mod render;
mod session;
mod skills;
mod tools;

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

use crate::cli::{AppMode, Args};
use crate::modes::interactive::run_interactive;
use crate::modes::print::run_print;

#[tokio::main]
async fn main() -> Result<()> {
    // Structured logging (env-controlled; warns only by default).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cwd = std::env::current_dir()?;

    // Separate @file references from the rest before clap parsing.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (files, rest): (Vec<String>, Vec<String>) = raw
        .into_iter()
        .partition(|a| a.starts_with('@') && !a.starts_with("@@") && a.len() > 1);

    // parse_from treats the first element as the binary name, so prepend one.
    let mut clap_args: Vec<String> = vec!["pi".to_string()];
    clap_args.extend(rest);
    let args = Args::parse_from(clap_args);

    // Inline @file contents into the initial message.
    let mut file_contents: Vec<String> = Vec::new();
    for f in &files {
        let path = &f[1..];
        match std::fs::read_to_string(path) {
            Ok(content) => file_contents.push(content),
            Err(e) => {
                eprintln!("warning: could not read @file {path}: {e}");
            }
        }
    }

    let stdin_is_tty = std::io::stdin().is_terminal();
    let app_mode = app::resolve_app_mode(&args, stdin_is_tty);
    let initial_message = app::build_initial_messages(&args.messages, &file_contents);

    match app_mode {
        AppMode::Print(output) => {
            // No positional prompt and stdin is piped (not a TTY): treat the
            // piped bytes as the prompt, e.g. `echo "explain this" | pi`.
            let initial_message = match initial_message {
                Some(m) => Some(m),
                None if !stdin_is_tty => app::read_stdin_prompt(),
                None => None,
            };
            if initial_message.is_none() {
                eprintln!(
                    "error: no prompt provided. Pass a message, pipe input, or run `pi` with no args for interactive mode."
                );
                std::process::exit(1);
            }
            let mut session = app::build_session(&args, &cwd, Vec::new())?;
            let code = run_print(&mut session, initial_message, output, args.verbose).await?;
            std::process::exit(code);
        }
        AppMode::Interactive => {
            let session = app::build_session(&args, &cwd, Vec::new())?;
            let code = run_interactive(session, initial_message, args.verbose).await?;
            std::process::exit(code);
        }
    }
}
