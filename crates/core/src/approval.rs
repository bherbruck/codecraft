use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use codecraft_protocol::{ApprovalDecision, ApprovalRequest};
use codecraft_tools::Access;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Ask before writes and non-read-only commands.
    #[default]
    Ask,
    /// File edits are free; commands ask.
    AutoEdit,
    /// Never ask.
    Yolo,
}

/// Asks the user. Implemented by front-ends.
#[async_trait]
pub trait Prompter: Send + Sync {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// Non-interactive: deny anything that would need a prompt.
pub struct DenyPrompter;
#[async_trait]
impl Prompter for DenyPrompter {
    async fn ask(&self, _: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// Shared by a session and its sub-agents, so "allow for session" carries over.
pub struct Approvals {
    pub mode: Mutex<ApprovalMode>,
    session_allowed: Mutex<HashSet<String>>,
    prompter: Box<dyn Prompter>,
    /// Serialize prompts so parallel calls don't interleave dialogs.
    gate: tokio::sync::Mutex<()>,
}

impl Approvals {
    pub fn new(mode: ApprovalMode, prompter: Box<dyn Prompter>) -> Self {
        Self { mode: Mutex::new(mode), session_allowed: Default::default(), prompter, gate: Default::default() }
    }

    pub fn allow_key(&self, key: &str) {
        self.session_allowed.lock().unwrap().insert(key.to_string());
    }

    /// Returns true if the call may proceed.
    pub async fn check(&self, tool: &str, access: &Access) -> bool {
        let mode = *self.mode.lock().unwrap();
        let (summary, key) = match access {
            Access::Read => return true,
            Access::Write { summary, key } => {
                if mode != ApprovalMode::Ask {
                    return true;
                }
                (summary, key)
            }
            Access::Exec { summary, key } => {
                if mode == ApprovalMode::Yolo {
                    return true;
                }
                (summary, key)
            }
        };
        let _g = self.gate.lock().await;
        if self.session_allowed.lock().unwrap().contains(key) {
            return true;
        }
        let req = ApprovalRequest {
            id: uuid::Uuid::new_v4().to_string(),
            tool: tool.to_string(),
            summary: summary.clone(),
            cache_key: key.clone(),
        };
        match self.prompter.ask(req).await {
            ApprovalDecision::Once => true,
            ApprovalDecision::Session => {
                self.allow_key(key);
                true
            }
            ApprovalDecision::Deny => false,
        }
    }
}
