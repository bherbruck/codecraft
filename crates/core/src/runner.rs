//! Executes tool calls: lookup, approval, parallel/exclusive gating, events.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use codecraft_llm::ToolHost;
use codecraft_protocol::{Event, ToolSpec};
use codecraft_tools::{FileLocks, Tool, ToolCtx, ToolOutput};
use serde_json::Value;
use tokio::sync::{mpsc::UnboundedSender, RwLock};
use tokio_util::sync::CancellationToken;

use crate::approval::Approvals;

const MAX_RESULT_BYTES: usize = 100 * 1024;

pub struct ToolRunner {
    pub session: String,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub order: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub approvals: Arc<Approvals>,
    pub events: UnboundedSender<Event>,
    pub file_locks: FileLocks,
    pub sandbox: Option<Arc<codecraft_tools::sandbox::Firecracker>>,
    gate: Arc<RwLock<()>>,
    /// Cancellation for the current turn (replaced each turn).
    pub cancel: std::sync::Mutex<CancellationToken>,
}

impl ToolRunner {
    pub fn new(
        session: String,
        tools: Vec<Arc<dyn Tool>>,
        cwd: std::path::PathBuf,
        approvals: Arc<Approvals>,
        events: UnboundedSender<Event>,
    ) -> Self {
        let order = tools.iter().map(|t| t.spec().name).collect();
        Self {
            session,
            tools: tools.into_iter().map(|t| (t.spec().name, t)).collect(),
            order,
            cwd,
            approvals,
            events,
            file_locks: FileLocks::default(),
            sandbox: None,
            gate: Arc::new(RwLock::new(())),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order.iter().filter_map(|n| self.tools.get(n)).map(|t| t.spec()).collect()
    }

    pub async fn execute(&self, id: &str, name: &str, args: Value) -> ToolOutput {
        let cancel = self.cancel.lock().unwrap().clone();
        let _ = self.events.send(Event::ToolStart {
            session: self.session.clone(),
            id: id.into(),
            name: name.into(),
            arguments: args.clone(),
        });
        let out = self.execute_inner(id, name, args, cancel).await;
        let _ = self.events.send(Event::ToolEnd {
            session: self.session.clone(),
            id: id.into(),
            name: name.into(),
            output: out.content.clone(),
            is_error: out.is_error,
        });
        out
    }

    async fn execute_inner(&self, id: &str, name: &str, args: Value, cancel: CancellationToken) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            return ToolOutput::err(format!("unknown tool `{name}`. Available: {}", self.order.join(", ")));
        };
        if let Some(bad) = args.get("__invalid_json") {
            return ToolOutput::err(format!("tool arguments were not valid JSON: {bad}"));
        }
        if cancel.is_cancelled() {
            return ToolOutput::err("aborted: the user interrupted before this ran");
        }
        let ctx = ToolCtx { cwd: self.cwd.clone(), cancel: cancel.clone(), call_id: id.into(), file_locks: self.file_locks.clone(), sandbox: self.sandbox.clone() };
        let access = tool.access(&args, &ctx);
        if !self.approvals.check(name, &access).await {
            return ToolOutput::err("The user denied this action. Do not retry it; ask what they want instead.");
        }
        let mut out = if tool.parallel_safe() {
            let _g = self.gate.read().await;
            tool.run(args, &ctx).await
        } else {
            let _g = self.gate.write().await;
            tool.run(args, &ctx).await
        };
        if out.content.len() > MAX_RESULT_BYTES {
            let mut end = MAX_RESULT_BYTES;
            while !out.content.is_char_boundary(end) {
                end -= 1;
            }
            out.content.truncate(end);
            out.content.push_str("\n[tool output truncated]");
        }
        out
    }
}

#[async_trait]
impl ToolHost for ToolRunner {
    async fn call(&self, id: &str, name: &str, args: Value) -> (String, bool) {
        let o = self.execute(id, name, args).await;
        (o.content, o.is_error)
    }
}
