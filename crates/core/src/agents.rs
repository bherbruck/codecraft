//! Sub-agent definitions: Markdown files with a small YAML-style frontmatter.
//!
//! ```text
//! ---
//! name: reviewer
//! description: Reviews diffs for bugs
//! model: anthropic/claude-sonnet-4-5   # optional
//! tools: read, grep, glob              # optional; default all
//! ---
//! You are a careful code reviewer...
//! ```

use std::path::Path;

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub prompt: String,
}

pub fn builtin() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "general".into(),
            description: "General-purpose agent for multi-step tasks: research, edits, running commands.".into(),
            model: None,
            tools: None,
            prompt: "You are a sub-agent handling one delegated task. Do the task completely, then reply with a \
                     concise report of what you found or changed (file paths, key facts). Your final message is \
                     all the parent agent sees."
                .into(),
        },
        AgentDef {
            name: "explore".into(),
            description: "Read-only codebase search. Use to find files, symbols and answer 'where/how is X' \
                          questions without filling the main context."
                .into(),
            model: None,
            tools: Some(vec!["read".into(), "grep".into(), "glob".into(), "ls".into()]),
            prompt: "You are a read-only exploration sub-agent. Search efficiently (glob/grep first, then read \
                     the relevant parts). Never modify anything. Reply with a concise answer: the conclusion, \
                     with file:line references, not file dumps."
                .into(),
        },
    ]
}

pub fn parse(text: &str) -> Option<AgentDef> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let (front, body) = (&rest[..end], &rest[end + 4..]);
    let mut def = AgentDef {
        name: String::new(),
        description: String::new(),
        model: None,
        tools: None,
        prompt: body.trim().to_string(),
    };
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
        match k.trim() {
            "name" => def.name = v,
            "description" => def.description = v,
            "model" if !v.is_empty() => def.model = Some(v),
            "tools" if !v.is_empty() => {
                let v = v.trim_start_matches('[').trim_end_matches(']');
                def.tools = Some(v.split(',').map(|s| s.trim().trim_matches('"').to_lowercase()).filter(|s| !s.is_empty()).collect())
            }
            _ => {}
        }
    }
    (!def.name.is_empty()).then_some(def)
}

/// Built-ins, then `~/.codecraft/agents/*.md`, then `<project>/.codecraft/agents/*.md`. Later wins.
pub fn load(cwd: &Path) -> Vec<AgentDef> {
    let mut all = builtin();
    let mut dirs = vec![crate::config::home_dir().join("agents")];
    if let Some(root) = crate::context::project_root(cwd) {
        dirs.push(root.join(".codecraft/agents"));
    }
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        paths.sort();
        for p in paths {
            if let Some(def) = std::fs::read_to_string(&p).ok().and_then(|t| parse(&t)) {
                all.retain(|a| a.name != def.name);
                all.push(def);
            }
        }
    }
    all
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let d = super::parse("---\nname: rev\ndescription: Reviews\ntools: [read, grep]\n---\nBe careful.\n").unwrap();
        assert_eq!(d.name, "rev");
        assert_eq!(d.tools.unwrap(), vec!["read", "grep"]);
        assert_eq!(d.prompt, "Be careful.");
    }
}
