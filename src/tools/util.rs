//! Shared tool helpers: path resolution and display.

use std::path::{Path, PathBuf};

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Resolve a tool-provided path against the working directory. Absolute paths
/// (and `~`-expanded paths) are used as-is; relative paths are joined to `cwd`.
pub fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Alias mirroring pi's `resolveReadPath` (same semantics as `resolveToCwd`).
pub fn resolve_read_path(path: &str, cwd: &Path) -> PathBuf {
    resolve_to_cwd(path, cwd)
}

/// Render a path compactly: relative to `cwd` when inside it, otherwise `~`-
/// abbreviated when inside the home dir.
pub fn shorten_path(path: &Path, cwd: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(cwd) {
        let s = rel.to_string_lossy().to_string();
        return if s.is_empty() { ".".to_string() } else { s };
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    path.to_string_lossy().to_string()
}

/// Stringify a path for display given the raw (possibly relative) input.
pub fn display_path(raw: &str, cwd: &Path) -> String {
    shorten_path(&resolve_to_cwd(raw, cwd), cwd)
}
