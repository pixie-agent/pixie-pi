//! Configuration paths and environment variables.

use std::path::{Path, PathBuf};

use crate::tools::util::expand_tilde;

/// Resolve the agent config directory (~/.config/pixie-pi/agent or `$PIXIE_PI_AGENT_DIR`).
pub fn agent_dir() -> PathBuf {
    if let Ok(env) = std::env::var("PIXIE_PI_AGENT_DIR") {
        if !env.is_empty() {
            return expand_tilde(&env);
        }
    }
    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pixie-pi").join("agent")
}

/// A short, stable directory name for a project (cwd). Mirrors pi's per-project
/// session grouping.
pub fn project_session_dir(cwd: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let abs = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = DefaultHasher::new();
    abs.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    agent_dir().join("sessions").join(format!("--{hash}--"))
}

/// Path to the active session JSONL file for a project.
pub fn session_file(cwd: &Path) -> PathBuf {
    project_session_dir(cwd).join("session.jsonl")
}
