//! Coding-agent tools (`packages/coding-agent`). The seven built-in tools plus
//! preset tool sets.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod file_mutex;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod skill;
pub mod truncate;
pub mod util;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::tool::AgentTool;

/// The full coding tool set: read, bash, edit, write (mirrors pi's
/// `createCodingTools`).
pub fn coding_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool { cwd: cwd.clone() }),
        Arc::new(bash::BashTool { cwd: cwd.clone() }),
        Arc::new(edit::EditTool { cwd: cwd.clone() }),
        Arc::new(write::WriteTool { cwd: cwd.clone() }),
    ]
}

/// Read-only tool set: read, grep, find, ls (mirrors `createReadOnlyTools`).
pub fn read_only_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool { cwd: cwd.clone() }),
        Arc::new(grep::GrepTool { cwd: cwd.clone() }),
        Arc::new(find::FindTool { cwd: cwd.clone() }),
        Arc::new(ls::LsTool { cwd }),
    ]
}

/// All seven built-in tools.
pub fn all_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool { cwd: cwd.clone() }),
        Arc::new(bash::BashTool { cwd: cwd.clone() }),
        Arc::new(edit::EditTool { cwd: cwd.clone() }),
        Arc::new(write::WriteTool { cwd: cwd.clone() }),
        Arc::new(grep::GrepTool { cwd: cwd.clone() }),
        Arc::new(find::FindTool { cwd: cwd.clone() }),
        Arc::new(ls::LsTool { cwd }),
    ]
}

/// Names of all built-in tools.
pub const BUILTIN_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];

/// Build a tool set from an allowlist of names.
pub fn select_tools(cwd: PathBuf, names: &[String]) -> Vec<Arc<dyn AgentTool>> {
    let all = all_tools(cwd);
    all.into_iter()
        .filter(|t| names.iter().any(|n| n == t.name()))
        .collect()
}
