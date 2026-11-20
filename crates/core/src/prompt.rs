use std::path::Path;

use codecraft_protocol::ToolSpec;

use crate::context::load_instructions;

pub struct PromptParts<'a> {
    pub cwd: &'a Path,
    pub tools: &'a [ToolSpec],
    pub agent_prompt: Option<&'a str>,
    pub agents: &'a [(String, String)],
}

pub fn build(p: PromptParts) -> String {
    let mut s = String::new();
    match p.agent_prompt {
        Some(a) => {
            s.push_str(a);
            s.push_str("\n\n");
        }
        None => s.push_str(
            "You are codecraft, a coding agent running in the user's terminal. You help with software \
             engineering tasks by reading code, editing files and running commands with the tools below.\n\n",
        ),
    }
    s.push_str("<tools>\n");
    for t in p.tools {
        let first = t.description.split(". ").next().unwrap_or("");
        s.push_str(&format!("- {}: {}\n", t.name, first.trim_end_matches('.')));
    }
    s.push_str("</tools>\n\n<rules>\n");
    for r in [
        "Read a file before editing it. Use `edit` for changes to existing files, `write` only for new files or full rewrites.",
        "Prefer `grep`/`glob`/`read` over shell equivalents.",
        "Run independent read-only tool calls in parallel in one response.",
        "Verify your work: build, run tests or re-read changed code when it makes sense.",
        "Be concise. Lead with the answer. Reference code as `path:line`.",
        "Don't make changes beyond what was asked. Never commit or push unless asked.",
    ] {
        s.push_str(&format!("- {r}\n"));
    }
    if !p.agents.is_empty() {
        s.push_str("- Delegate broad searches or independent sub-tasks to sub-agents with the `task` tool; several `task` calls in one response run in parallel.\n");
    }
    s.push_str("</rules>\n");
    if !p.agents.is_empty() {
        s.push_str("\n<subagents>\n");
        for (name, desc) in p.agents {
            s.push_str(&format!("- {name}: {desc}\n"));
        }
        s.push_str("</subagents>\n");
    }
    let files = load_instructions(p.cwd);
    if !files.is_empty() {
        s.push_str("\n<project_context>\nInstructions from the user and project. Follow them.\n");
        for f in files {
            s.push_str(&format!(
                "<instructions path=\"{}\">\n{}\n</instructions>\n",
                f.path.display(),
                f.content.trim()
            ));
        }
        s.push_str("</project_context>\n");
    }
    s.push_str(&format!(
        "\n<environment>\ncwd: {}\nos: {}\ndate: {}\ngit repo: {}\n</environment>\n",
        p.cwd.display(),
        std::env::consts::OS,
        chrono::Local::now().format("%Y-%m-%d"),
        crate::context::project_root(p.cwd).is_some(),
    ));
    s
}
