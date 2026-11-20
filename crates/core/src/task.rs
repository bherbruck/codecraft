//! The `task` tool: delegate work to a sub-agent running in its own session.


use async_trait::async_trait;
use codecraft_protocol::{Event, Message, ToolSpec};
use codecraft_tools::{Access, Tool, ToolCtx, ToolOutput};
use serde_json::{json, Value};

use crate::engine::{Session, SessionSetup};

pub struct TaskTool {
    setup: SessionSetup,
    parent_id: String,
}

impl TaskTool {
    pub fn new(setup: SessionSetup, parent_id: String) -> Self {
        Self { setup, parent_id }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        let names: Vec<&str> = self.setup.agents.iter().map(|a| a.name.as_str()).collect();
        let list: String =
            self.setup.agents.iter().map(|a| format!("\n- {}: {}", a.name, a.description)).collect();
        ToolSpec {
            name: "task".into(),
            description: format!(
                "Run a sub-agent on a self-contained task. It starts with an empty context, works on its own, \
                 and returns only its final report, which keeps your context small. Give it a complete brief: \
                 goal, relevant paths, what you already know, what to return. Several task calls in one \
                 response run in parallel. Agents:{list}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "enum": names, "description": "Which sub-agent to run"},
                    "description": {"type": "string", "description": "3-6 word label shown to the user"},
                    "prompt": {"type": "string", "description": "The full brief for the sub-agent"}
                },
                "required": ["agent", "description", "prompt"]
            }),
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn access(&self, _: &Value, _: &ToolCtx) -> Access {
        // The sub-agent's own tool calls go through approvals.
        Access::Read
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let name = args["agent"].as_str().unwrap_or("general");
        let Some(def) = self.setup.agents.iter().find(|a| a.name == name).cloned() else {
            return ToolOutput::err(format!("unknown agent `{name}`"));
        };
        let Some(prompt) = args["prompt"].as_str() else {
            return ToolOutput::err("missing `prompt`");
        };
        let description = args["description"].as_str().unwrap_or(name).to_string();
        let mut setup = self.setup.clone();
        setup.model = def.model.clone().unwrap_or_else(|| self.setup.model.clone());
        setup.agent = Some(def);
        setup.depth = self.setup.depth + 1;
        setup.parent_id = Some(self.parent_id.clone());
        setup.id = None;
        setup.messages = vec![];
        let mut child = match Session::new(setup) {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(format!("could not start sub-agent: {e:#}")),
        };
        let _ = self.setup.events.send(Event::SubagentStarted {
            session: child.id.clone(),
            parent: self.parent_id.clone(),
            call_id: ctx.call_id.clone(),
            agent: name.into(),
            description,
        });
        let res = child.run(prompt, ctx.cancel.child_token()).await;
        let _ = self.setup.events.send(Event::SubagentFinished {
            session: child.id.clone(),
            parent: self.parent_id.clone(),
            call_id: ctx.call_id.clone(),
        });
        let calls = child
            .messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult { .. }))
            .count();
        let text = child.last_text();
        match res {
            Ok(()) if !text.is_empty() => {
                ToolOutput::ok(format!("{text}\n\n[sub-agent `{name}` finished after {calls} tool calls]"))
            }
            Ok(()) => ToolOutput::err(format!("sub-agent `{name}` returned no text after {calls} tool calls")),
            Err(e) => ToolOutput::err(format!(
                "sub-agent `{name}` failed after {calls} tool calls: {e:#}\nPartial output:\n{text}"
            )),
        }
    }
}
