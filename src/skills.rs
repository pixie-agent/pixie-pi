//! Skill discovery and parsing — Claude Code–compatible.
//!
//! A skill is a directory `<root>/<name>/SKILL.md` where `<root>` is
//! `.claude/skills` (project) or `~/.claude/skills` (user). The `SKILL.md`
//! begins with a small YAML frontmatter (`name`, `description`) followed by the
//! markdown body. Following Claude Code's progressive-disclosure model, only the
//! name + description are surfaced up front (in the system prompt); the body is
//! loaded into context only when the model invokes the `skill` tool.
//!
//! Other frontmatter keys (e.g. Claude Code's `allowed-tools`) are ignored:
//! skills do not restrict the tool set here — every registered tool remains
//! callable while a skill runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where a skill was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `<cwd>/.claude/skills` — takes precedence over user skills.
    Project,
    /// `~/.claude/skills`.
    User,
}

/// One discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The markdown body (after the frontmatter).
    pub body: String,
    /// The skill's directory — supporting files live here.
    pub dir: PathBuf,
    pub source: SkillSource,
}

/// The set of discovered skills.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    pub skills: Vec<Skill>,
}

impl Skills {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Discover skills in `<cwd>/.claude/skills` (project) and
    /// `~/.claude/skills` (user). Project skills shadow user skills of the same
    /// name. Returns an empty registry if neither directory exists.
    pub fn discover(cwd: &Path) -> Self {
        let mut roots: Vec<(PathBuf, SkillSource)> =
            vec![(cwd.join(".claude").join("skills"), SkillSource::Project)];
        if let Some(home) = dirs::home_dir() {
            roots.push((home.join(".claude").join("skills"), SkillSource::User));
        }
        Self::discover_from_roots(&roots)
    }

    /// Discovery over an explicit list of `(root, source)`, earlier roots
    /// shadowing later ones. Separated out so tests can supply synthetic roots
    /// (including a fake "user" dir) without touching the real home directory.
    pub fn discover_from_roots(roots: &[(PathBuf, SkillSource)]) -> Self {
        let mut skills: Vec<Skill> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (root, source) in roots {
            if !root.is_dir() {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(root) else {
                continue;
            };
            let mut entries: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
            entries.sort();
            for dir in entries {
                if !dir.is_dir() {
                    continue;
                }
                let skill_md = dir.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                let Some(skill) = parse_skill_file(&skill_md, &dir, *source) else {
                    continue;
                };
                if seen.insert(skill.name.clone()) {
                    skills.push(skill);
                }
            }
        }
        Skills { skills }
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }
}

/// Read + parse a `SKILL.md`. Falls back to the directory name and an empty
/// description when frontmatter is absent or unparsable.
fn parse_skill_file(path: &Path, dir: &Path, source: SkillSource) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&raw);
    let parsed = fm.as_deref().map(parse_frontmatter);
    let dir_name = dir.file_name()?.to_string_lossy().to_string();
    let name = parsed
        .as_ref()
        .map(|p| p.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or(dir_name);
    let description = parsed
        .as_ref()
        .map(|p| p.description.clone())
        .unwrap_or_default();
    Some(Skill {
        name,
        description,
        body,
        dir: dir.to_path_buf(),
        source,
    })
}

#[derive(Debug, Default)]
struct ParsedFrontmatter {
    name: String,
    description: String,
}

/// Split a `SKILL.md` into `(frontmatter, body)`. Frontmatter is delimited by a
/// leading `---` line and a closing `---` (or `...`) line. Returns
/// `(None, whole_file)` when there is no frontmatter.
fn split_frontmatter(raw: &str) -> (Option<String>, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let lines: Vec<&str> = raw.split('\n').collect();
    if lines.first().map(|l| l.trim() == "---").unwrap_or(false) {
        if let Some(rel) = lines
            .iter()
            .skip(1)
            .position(|l| l.trim() == "---" || l.trim() == "...")
        {
            let close = rel + 1;
            let fm = lines[1..close].join("\n");
            let body = lines
                .get(close + 1..)
                .map(|s| s.join("\n"))
                .unwrap_or_default();
            return (Some(fm), body);
        }
    }
    (None, raw.to_string())
}

/// Parse the small subset of YAML frontmatter skills use: scalar `name` and
/// `description`. Every other key (and all indented/list lines) is ignored —
/// we deliberately don't model `allowed-tools` or other fields.
fn parse_frontmatter(fm: &str) -> ParsedFrontmatter {
    let mut out = ParsedFrontmatter::default();
    for line in fm.lines() {
        // Only top-level scalar keys; skip indented lines (lists, nested maps).
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "name" => out.name = unquote(v.trim()),
                "description" => out.description = unquote(v.trim()),
                _ => {}
            }
        }
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
{
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        dir
    }

    #[test]
    fn parses_name_description_and_body() {
        let md = "---\nname: my-skill\ndescription: Use when foo\n---\n# Steps\nDo the thing.\n";
        let (fm, body) = split_frontmatter(md);
        let p = parse_frontmatter(&fm.unwrap());
        assert_eq!(p.name, "my-skill");
        assert_eq!(p.description, "Use when foo");
        assert!(body.contains("# Steps"));
        assert!(body.contains("Do the thing."));
    }

    #[test]
    fn ignores_unknown_frontmatter_keys_like_allowed_tools() {
        // `allowed-tools` (and any other unknown key) must be ignored — skills
        // don't restrict the tool set, so we don't even parse it. name/description
        // still parse correctly alongside it.
        let p = parse_frontmatter("name: a\ndescription: do a thing\nallowed-tools: [bash, read]\n");
        assert_eq!(p.name, "a");
        assert_eq!(p.description, "do a thing");
    }

    #[test]
    fn strips_quotes_around_values() {
        let p = parse_frontmatter("name: \"quoted-name\"\ndescription: 'single quoted'\n");
        assert_eq!(p.name, "quoted-name");
        assert_eq!(p.description, "single quoted");
    }

    #[test]
    fn no_frontmatter_falls_back_to_dir_name() {
        let tmp = std::env::temp_dir().join(format!("pi-skills-nofm-{}", uuid::Uuid::new_v4()));
        let dir = write_skill(&tmp, "plain-skill", "Just a body, no frontmatter.\n");
        let skill = parse_skill_file(&dir.join("SKILL.md"), &dir, SkillSource::Project).unwrap();
        assert_eq!(skill.name, "plain-skill");
        assert_eq!(skill.description, "");
        assert!(skill.body.contains("Just a body"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_finds_project_and_user_skills() {
        let project = std::env::temp_dir().join(format!("pi-skills-proj-{}", uuid::Uuid::new_v4()));
        let user = std::env::temp_dir().join(format!("pi-skills-user-{}", uuid::Uuid::new_v4()));
        write_skill(&project, "alpha", "---\nname: alpha\ndescription: a\n---\nbody-a\n");
        write_skill(&user, "beta", "---\nname: beta\ndescription: b\n---\nbody-b\n");
        let skills = Skills::discover_from_roots(&[
            (project.clone(), SkillSource::Project),
            (user.clone(), SkillSource::User),
        ]);
        let names = skills.names();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"beta"), "{names:?}");
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&user);
    }

    #[test]
    fn project_skill_shadows_user_skill_of_same_name() {
        let project = std::env::temp_dir().join(format!("pi-skills-sp-{}", uuid::Uuid::new_v4()));
        let user = std::env::temp_dir().join(format!("pi-skills-su-{}", uuid::Uuid::new_v4()));
        write_skill(&project, "shared", "---\nname: shared\ndescription: PROJECT\n---\nproj-body\n");
        write_skill(&user, "shared", "---\nname: shared\ndescription: USER\n---\nuser-body\n");
        let skills = Skills::discover_from_roots(&[
            (project.clone(), SkillSource::Project),
            (user.clone(), SkillSource::User),
        ]);
        // Only one "shared", and it's the project one.
        assert_eq!(skills.skills.iter().filter(|s| s.name == "shared").count(), 1);
        assert_eq!(skills.find("shared").unwrap().description, "PROJECT");
        assert_eq!(skills.find("shared").unwrap().source, SkillSource::Project);
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&user);
    }

    #[test]
    fn discover_ignores_files_and_dirs_without_skill_md() {
        let root = std::env::temp_dir().join(format!("pi-skills-skip-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("not-a-skill")).unwrap(); // no SKILL.md
        std::fs::write(root.join("loose-file.md"), "x").unwrap(); // not a dir
        write_skill(&root, "real", "---\nname: real\ndescription: r\n---\nb\n");
        let skills = Skills::discover_from_roots(&[(root.clone(), SkillSource::Project)]);
        assert_eq!(skills.names(), vec!["real"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
