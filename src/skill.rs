//! The self-skill: a SKILL.md that teaches coding agents on this machine how
//! to drive BOMvoyage, installed into every detected agent's skills
//! directory by `bomvoyage skill install`.
//!
//! The self-skill is addressable as `self` (canonical) or by the tool's own
//! name (`bomvoyage`). Passing an agent name as the positional is a hard
//! error — use `--agent <name>`. Agents are detected by the presence of
//! their configuration directory under the home directory, so a
//! `skill install` with no argument covers every agent on the machine in
//! one go.

use std::path::{Path, PathBuf};

use crate::error::{ToolError, ToolResult};

pub const SKILL_MD: &str = include_str!("skill/bomvoyage-SKILL.md");
pub const SKILL_NAME: &str = "bomvoyage";

/// One supported agent: `key` is the `--agent` spelling; `roots` are
/// candidate config directories under `$HOME` (detection = any exists;
/// the first existing one hosts the skills directory).
pub struct AgentSpec {
    pub key: &'static str,
    pub display: &'static str,
    pub roots: &'static [&'static str],
}

pub const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        key: "claude-code",
        display: "Claude Code",
        roots: &[".claude"],
    },
    AgentSpec {
        key: "cursor",
        display: "Cursor",
        roots: &[".cursor"],
    },
    AgentSpec {
        key: "windsurf",
        display: "Windsurf",
        roots: &[".codeium/windsurf", ".windsurf"],
    },
    AgentSpec {
        key: "copilot",
        display: "Copilot",
        roots: &[".copilot"],
    },
    AgentSpec {
        key: "antigravity",
        display: "Antigravity",
        roots: &[".antigravity"],
    },
    AgentSpec {
        key: "gemini-cli",
        display: "Gemini CLI",
        roots: &[".gemini"],
    },
    AgentSpec {
        key: "cline",
        display: "Cline",
        roots: &[".cline"],
    },
    AgentSpec {
        key: "codex",
        display: "Codex",
        roots: &[".codex"],
    },
    AgentSpec {
        key: "pi",
        display: "Pi",
        roots: &[".pi"],
    },
    AgentSpec {
        key: "opencode",
        display: "OpenCode",
        roots: &[".opencode", ".config/opencode"],
    },
    AgentSpec {
        key: "junie",
        display: "Junie",
        roots: &[".junie"],
    },
    AgentSpec {
        key: "kiro",
        display: "Kiro",
        roots: &[".kiro"],
    },
];

pub fn home_dir() -> ToolResult<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| ToolError::Other("no home directory (set HOME)".into()))?;
    if home.is_empty() {
        return Err(ToolError::Other("no home directory (HOME is empty)".into()));
    }
    Ok(PathBuf::from(home))
}

/// `--agent` matching: kebab key, case- and separator-insensitive
/// (`claude code`, `Claude_Code` and `claude-code` all resolve).
fn normalize(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

pub fn resolve_agent(name: &str) -> Option<&'static AgentSpec> {
    let want = normalize(name);
    AGENTS.iter().find(|a| a.key == want)
}

/// The agent's skills directory under `home` — the first *existing* config
/// root wins, so nothing is created for agents that aren't on the machine
/// (unless the caller decides otherwise for an explicit `--agent`).
pub fn skills_dir_for(home: &Path, spec: &AgentSpec) -> Option<PathBuf> {
    spec.roots
        .iter()
        .map(|r| home.join(r))
        .find(|root| root.is_dir())
        .map(|root| root.join("skills").join(SKILL_NAME))
}

/// Every agent detected on this machine with its skills directory.
pub fn detected(home: &Path) -> Vec<(&'static AgentSpec, PathBuf)> {
    AGENTS
        .iter()
        .filter_map(|spec| skills_dir_for(home, spec).map(|dir| (spec, dir)))
        .collect()
}

/// The positional must address the self-skill — never an agent.
pub fn check_skill_ref(skill_ref: &str) -> ToolResult<()> {
    match normalize(skill_ref).as_str() {
        "self" | "" | SKILL_NAME => Ok(()),
        other => {
            if resolve_agent(other).is_some() {
                Err(ToolError::Other(format!(
                    "'{skill_ref}' is an agent, not a skill — pass it as --agent {other} instead"
                )))
            } else {
                Err(ToolError::Other(format!(
                    "unknown skill '{skill_ref}' — the self-skill is addressable as 'self' (or '{SKILL_NAME}'); to target an agent, use --agent <name>"
                )))
            }
        }
    }
}

/// Write the skill into one agent's skills directory. Returns the SKILL.md
/// path and whether an existing skill was replaced.
fn install_into(skills_dir: &Path) -> ToolResult<(PathBuf, bool)> {
    let replaced = skills_dir.join("SKILL.md").is_file();
    std::fs::create_dir_all(skills_dir)
        .map_err(|e| ToolError::Other(format!("cannot create {}: {e}", skills_dir.display())))?;
    let dest = skills_dir.join("SKILL.md");
    std::fs::write(&dest, SKILL_MD)
        .map_err(|e| ToolError::Other(format!("cannot write {}: {e}", dest.display())))?;
    Ok((dest, replaced))
}

/// `bomvoyage skill install [self] [--agent NAME]`.
pub fn install_command(skill_ref: &str, agent: Option<&str>) -> ToolResult<()> {
    check_skill_ref(skill_ref)?;
    let home = home_dir()?;

    if let Some(name) = agent {
        let spec = resolve_agent(name).ok_or_else(|| {
            let keys: Vec<&str> = AGENTS.iter().map(|a| a.key).collect();
            ToolError::Other(format!(
                "unknown agent '{name}' — supported: {}",
                keys.join(", ")
            ))
        })?;
        let skills_dir = skills_dir_for(&home, spec)
            .unwrap_or_else(|| home.join(spec.roots[0]).join("skills").join(SKILL_NAME));
        let (path, replaced) = install_into(&skills_dir)?;
        println!(
            "{} skill '{SKILL_NAME}' for {}: {}",
            if replaced { "updated" } else { "installed" },
            spec.display,
            path.display()
        );
        return Ok(());
    }

    let found = detected(&home);
    if found.is_empty() {
        eprintln!(
            "no supported coding agents detected on this machine (looking for: {})",
            AGENTS.iter().map(|a| a.key).collect::<Vec<_>>().join(", ")
        );
        eprintln!("install one explicitly with: bomvoyage skill install --agent <name>");
        return Ok(());
    }
    let not_detected = AGENTS.len() - found.len();
    println!(
        "detected {} coding agent(s), installing self-skill:",
        found.len()
    );
    for (spec, skills_dir) in &found {
        let (path, replaced) = install_into(skills_dir)?;
        println!(
            "  {} {} — {}",
            if replaced { "updated" } else { "installed" },
            spec.display,
            path.display()
        );
    }
    if not_detected > 0 {
        let missing: Vec<&str> = AGENTS
            .iter()
            .filter(|a| skills_dir_for(&home, a).is_none())
            .map(|a| a.key)
            .collect();
        eprintln!(
            "not detected on this machine (skipped): {} — install explicitly with --agent <name>",
            missing.join(", ")
        );
    }
    Ok(())
}

/// `bomvoyage skill list`.
pub fn list_command() -> ToolResult<()> {
    let home = home_dir()?;
    println!("{:<14} {:<14} SKILL PATH", "AGENT", "DETECTED");
    for spec in AGENTS {
        let dir = skills_dir_for(&home, spec);
        let installed = dir
            .as_ref()
            .map(|d| d.join("SKILL.md").is_file())
            .unwrap_or(false);
        println!(
            "{:<14} {:<14} {}",
            spec.key,
            if dir.is_some() { "yes" } else { "no" },
            match (&dir, installed) {
                (Some(d), true) => format!("{} (installed)", d.join("SKILL.md").display()),
                (Some(d), false) => format!("{} (not installed)", d.join("SKILL.md").display()),
                (None, _) => "—".to_string(),
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bomvoyage-skill-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn positional_rejects_agent_names() {
        // the hard error: an agent name as the positional
        for name in ["cursor", "claude-code", "Claude Code", "codex", "kiro"] {
            let err = check_skill_ref(name).unwrap_err().to_string();
            assert!(err.contains("--agent"), "should point at --agent: {err}");
        }
        // the self-skill, both spellings, any case
        assert!(check_skill_ref("self").is_ok());
        assert!(check_skill_ref("SELF").is_ok());
        assert!(check_skill_ref("bomvoyage").is_ok());
        assert!(check_skill_ref("BOMvoyage").is_ok());
        // unknown skill: same hard error, still pointing at --agent
        let err = check_skill_ref("wat").unwrap_err().to_string();
        assert!(err.contains("--agent") || err.contains("self"));
    }

    #[test]
    fn detection_by_config_dir_presence() {
        let home = scratch("detection");
        assert!(detected(&home).is_empty());

        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
        let found = detected(&home);
        assert_eq!(found.len(), 2);
        let keys: Vec<_> = found.iter().map(|(s, _)| s.key).collect();
        assert!(keys.contains(&"claude-code"));
        assert!(keys.contains(&"opencode")); // via .config/opencode root
        let (_, oc) = found.iter().find(|(s, _)| s.key == "opencode").unwrap();
        assert!(oc.starts_with(home.join(".config/opencode")));

        // ~/.opencode takes precedence for opencode when both exist
        std::fs::create_dir_all(home.join(".opencode")).unwrap();
        let (_, oc) = detected(&home)
            .into_iter()
            .find(|(s, _)| s.key == "opencode")
            .unwrap();
        assert!(oc.starts_with(home.join(".opencode")));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn install_writes_skill_and_reports_replacement() {
        let home = scratch("install");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        let spec = resolve_agent("Cursor").expect("cursor");
        let dir = skills_dir_for(&home, spec).expect("cursor detected with .cursor present");
        let (path, replaced) = install_into(&dir).unwrap();
        assert!(!replaced);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: bomvoyage"));
        assert!(text.contains("update apply"));
        let (_, replaced2) = install_into(&dir).unwrap();
        assert!(replaced2);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn agent_names_normalize() {
        assert_eq!(resolve_agent("claude code").unwrap().key, "claude-code");
        assert_eq!(resolve_agent("Gemini_CLI").unwrap().key, "gemini-cli");
        assert!(resolve_agent("nope").is_none());
    }
}
