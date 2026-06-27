//! CLI argument parsing (`cli/args.ts`). Mirrors the subset of pi flags that
//! the two supported modes (command-line / interactive) need.

use clap::Parser;

/// pixie-pi — an AI coding agent with read/write/edit/bash/grep/find/ls tools.
///
/// Run with no prompt to enter the interactive REPL, or pass a prompt (or
/// `-p`/`--print`) for a one-shot command-line run.
#[derive(Parser, Debug)]
#[command(
    name = "pixie-pi",
    version,
    about = "pixie-pi — AI coding agent (command-line and interactive modes)"
)]
pub struct Args {
    /// Prompt message(s). Omit to start the interactive REPL.
    #[arg(value_name = "MESSAGE")]
    pub messages: Vec<String>,

    /// Non-interactive: process the prompt and exit (command-line mode).
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// Output mode for command-line runs: text (default) or json (NDJSON events).
    #[arg(long, value_name = "text|json", default_value = "text")]
    pub mode: String,

    /// Model id or `provider/id` (optionally suffixed `:thinking`).
    #[arg(long)]
    pub model: Option<String>,

    /// Provider name (default: anthropic).
    #[arg(long)]
    pub provider: Option<String>,

    /// API key (defaults to ANTHROPIC_API_KEY).
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Replace the system prompt entirely.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Append text to the system prompt (repeatable).
    #[arg(long = "append-system-prompt", value_name = "TEXT")]
    pub append_system_prompt: Vec<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh.
    #[arg(long)]
    pub thinking: Option<String>,

    /// Continue the most recent session for this project.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a previous session (most recent in this project).
    #[arg(short = 'r', long = "resume")]
    pub resume: bool,

    /// Use a specific session file or id prefix.
    #[arg(long)]
    pub session: Option<String>,

    /// Don't persist the session (ephemeral).
    #[arg(long = "no-session")]
    pub no_session: bool,

    /// Comma-separated allowlist of tool names to enable.
    #[arg(short = 't', long = "tools", value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Comma-separated denylist of tool names to disable.
    #[arg(long = "exclude-tools", value_delimiter = ',')]
    pub exclude_tools: Vec<String>,

    /// Disable all tools.
    #[arg(short = 'n', long = "no-tools")]
    pub no_tools: bool,

    /// Disable built-in tools.
    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,

    /// Disable Anthropic prompt caching.
    #[arg(long = "no-cache")]
    pub no_cache: bool,

    /// Override the model's maximum output tokens.
    #[arg(long = "max-tokens")]
    pub max_tokens: Option<usize>,

    /// Show model thinking output while streaming.
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Interactive,
    Print(OutputMode),
}
