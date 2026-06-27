//! System prompt builder for the coding agent.

use std::path::Path;

use crate::skills::Skills;

/// Options for building the system prompt.
pub struct PromptOptions<'a> {
    pub cwd: &'a Path,
    pub tool_names: &'a [String],
    pub extra: Option<&'a str>,
    /// Discovered skills, if any (surfaced by name + description only).
    pub skills: Option<&'a Skills>,
}

/// Build the coding-assistant system prompt, including live environment info.
pub fn build_system_prompt(opts: PromptOptions<'_>) -> String {
    let platform = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unix"
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "cmd.exe".into()
        } else {
            "/bin/sh".into()
        }
    });
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let cwd_display = opts.cwd.to_string_lossy();

    let tool_list = if opts.tool_names.is_empty() {
        "(no tools available)".to_string()
    } else {
        opts.tool_names.join(", ")
    };

    let mut prompt = format!(
        "You are pixie-pi, an expert software engineering agent operating in a terminal. \
You help the user with coding tasks: reading, writing, and editing code, running commands, and searching the codebase. \
You work autonomously and use your tools to accomplish the task, then report the outcome concisely.

# Working directory
{cwd_display}

# Environment
- Platform: {platform}
- Shell: {shell}
- Date: {date}
- Tools: {tool_list}

# How to work
- Prefer the smallest correct change. Use `edit` for precise changes to existing files; use `write` only for new files or complete rewrites.
- Read a file before editing it so your oldText matches exactly (including indentation and whitespace).
- `edit` applies all entries in `edits[]` against the ORIGINAL file, not incrementally. Each `oldText` must be unique and non-overlapping. Merge nearby changes into one edit.
- Use `read` (not cat/sed) to inspect files. Use `grep` to search contents and `find` to locate files by name — both respect .gitignore.
- Use `bash` to run commands, build, and test. Inspect output before declaring success; if a command fails, read the error and fix it.
- Think step by step when the task is non-trivial, but do not narrate every action.
- Stay within the working directory; do not modify files outside it unless explicitly asked.

# Output style
- Be concise. No filler (\"Great question!\", \"Certainly!\"). State what you did and what to verify.
- When the task is done, summarize the result and any next steps. If you hit a blocker, say so plainly.
"
    );

    if let Some(extra) = opts.extra {
        prompt.push_str("\n# Additional instructions\n");
        prompt.push_str(extra);
        prompt.push('\n');
    }

    if let Some(skills) = opts.skills {
        if !skills.skills.is_empty() {
            prompt.push_str("\n# Skills\n\n");
            prompt.push_str(
                "Skills are on-demand capability modules. Each is listed with its name and a \
                 short description of when to use it. When the user's request matches a skill, \
                 invoke it with the `skill` tool — that loads the skill's full instructions, \
                 which you then follow. Do not invoke a skill unless it is relevant. A skill may \
                 reference supporting files (scripts, references) under its own directory; read \
                 those with the `read` tool.\n\nAvailable skills:\n",
            );
            for s in &skills.skills {
                let desc = if s.description.is_empty() {
                    "(no description)"
                } else {
                    s.description.as_str()
                };
                prompt.push_str(&format!("- **{}**: {}\n", s.name, desc));
            }
        }
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillSource};
    use std::path::PathBuf;

    #[test]
    fn prompt_lists_discovered_skills_by_name_and_description() {
        let skills = Skills {
            skills: vec![Skill {
                name: "deploy".into(),
                description: "How to ship to prod".into(),
                body: String::new(),
                dir: PathBuf::from("."),
                source: SkillSource::Project,
            }],
        };
        let p = build_system_prompt(PromptOptions {
            cwd: std::path::Path::new("."),
            tool_names: &["read".to_string()],
            extra: None,
            skills: Some(&skills),
        });
        assert!(p.contains("# Skills"), "skills section present");
        assert!(p.contains("**deploy**"), "skill name listed");
        assert!(p.contains("How to ship to prod"), "skill description listed");
        assert!(p.contains("`skill` tool"), "guidance to invoke via skill tool");
    }

    #[test]
    fn prompt_has_no_skills_section_when_none() {
        let p = build_system_prompt(PromptOptions {
            cwd: std::path::Path::new("."),
            tool_names: &["read".to_string()],
            extra: None,
            skills: None,
        });
        assert!(!p.contains("# Skills"));
    }
}
