//! `skill` tool — invoke a discovered skill to load its instructions.
//!
//! Claude Code–compatible: the model calls `skill` with a skill name (and
//! optional args); the tool returns the skill's `SKILL.md` body, which the model
//! then follows. Only descriptions are kept in the system prompt; the body is
//! loaded on demand (progressive disclosure).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::skills::Skills;

pub struct SkillTool {
    #[allow(dead_code)]
    pub cwd: PathBuf,
    pub skills: Arc<Skills>,
    desc: String,
}

impl SkillTool {
    pub fn new(cwd: PathBuf, skills: Arc<Skills>) -> Self {
        let names = skills
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let desc = format!(
            "Invoke a skill by name to load its full instructions into context, then follow them. \
             Only call this when the user's request matches a skill. Available skills: {names}"
        );
        Self { cwd, skills, desc }
    }
}

#[async_trait]
impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        &self.desc
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Name of the skill to invoke" },
                "args": { "type": "string", "description": "Optional arguments to pass to the skill" }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, args: Value, _cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let name = args
            .get("skill")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("skill requires a 'skill' name parameter"))?;
        let skill = match self.skills.find(name) {
            Some(s) => s,
            None => {
                let avail = self.skills.names().join(", ");
                anyhow::bail!("Unknown skill '{name}'. Available skills: {avail}");
            }
        };
        let mut out = format!("# Skill: {}\n\n", skill.name);
        if let Some(a) = args.get("args").and_then(|v| v.as_str()) {
            if !a.trim().is_empty() {
                out.push_str(&format!("Arguments: {a}\n\n"));
            }
        }
        out.push_str(skill.body.trim());
        out.push_str(&format!(
            "\n\n---\nSkill directory: `{}`. Reference any supporting files (scripts, references) \
             by their path under this directory using the read or bash tools.",
            skill.dir.display()
        ));
        Ok(ToolResult::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillSource};

    fn registry_with(skill: Skill) -> Arc<Skills> {
        Arc::new(Skills { skills: vec![skill] })
    }

    async fn run_skill(tool: &SkillTool, args: Value) -> Result<String, String> {
        match tool.execute(args, CancellationToken::new()).await {
            Ok(r) => match r.content.into_iter().next() {
                Some(crate::ai::types::ToolResultContent::Text { text }) => Ok(text),
                _ => panic!("expected text result"),
            },
            Err(e) => Err(e.to_string()),
        }
    }

    #[tokio::test]
    async fn returns_body_for_known_skill_and_surfaces_dir() {
        let dir = std::env::temp_dir().join(format!("pi-skilltool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let skill = Skill {
            name: "deploy".into(),
            description: "How to deploy".into(),
            body: "Run `make deploy` then verify.".into(),
            dir: dir.clone(),
            source: SkillSource::Project,
        };
        let tool = SkillTool::new(PathBuf::from("."), registry_with(skill));
        let out = run_skill(&tool, json!({ "skill": "deploy", "args": "prod" }))
            .await
            .unwrap();
        assert!(out.contains("# Skill: deploy"), "{out}");
        assert!(out.contains("Run `make deploy`"), "{out}");
        assert!(out.contains("Arguments: prod"), "{out}");
        // The skill directory is surfaced so the model can read supporting files.
        assert!(out.contains(&dir.display().to_string()), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_skill_lists_available() {
        let skill = Skill {
            name: "deploy".into(),
            description: "d".into(),
            body: "b".into(),
            dir: PathBuf::from("."),
            source: SkillSource::Project,
        };
        let tool = SkillTool::new(PathBuf::from("."), registry_with(skill));
        let err = run_skill(&tool, json!({ "skill": "nope" })).await.unwrap_err();
        assert!(err.contains("Unknown skill 'nope'"), "{err}");
        assert!(err.contains("deploy"), "should list available: {err}");
    }

    #[tokio::test]
    async fn missing_skill_param_errors() {
        let tool = SkillTool::new(PathBuf::from("."), Arc::new(Skills::empty()));
        let err = run_skill(&tool, json!({})).await.unwrap_err();
        assert!(err.contains("requires a 'skill'"), "{err}");
    }
}
