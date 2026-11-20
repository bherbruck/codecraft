//! Pure data types shared by every codecraft crate. No logic lives here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One block of message content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall { id: String, name: String, arguments: Value },
    Image { media_type: String, data: String },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// USD, if known.
    pub cost: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
    }
    /// Tokens occupying the context window after this response.
    pub fn context_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, arguments } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
    }
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User { content: Vec<ContentBlock> },
    Assistant(AssistantMessage),
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User { content: vec![ContentBlock::text(text)] }
    }
    pub fn approx_tokens(&self) -> u64 {
        let s = serde_json::to_string(self).unwrap_or_default();
        (s.len() as u64) / 4
    }
}

/// JSON-schema tool description sent to models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

/// Streaming events a provider emits while sampling.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, json: String },
}

// ---------- engine <-> front-end ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    /// Delivered at the next turn boundary, while the agent is running.
    Steer,
    /// Delivered once the agent would otherwise stop.
    FollowUp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Session,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool: String,
    pub summary: String,
    /// Key used for "approve for session" caching.
    pub cache_key: String,
}

/// Events the engine emits. `session` identifies the (sub-)session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted { session: String },
    TextDelta { session: String, text: String },
    ThinkingDelta { session: String, text: String },
    AssistantMessage { session: String, message: AssistantMessage },
    ToolStart { session: String, id: String, name: String, arguments: Value },
    ToolEnd { session: String, id: String, name: String, output: String, is_error: bool },
    SubagentStarted { session: String, parent: String, call_id: String, agent: String, description: String },
    SubagentFinished { session: String, parent: String, call_id: String },
    Compacted { session: String, tokens_before: u64 },
    Retrying { session: String, attempt: u32, error: String },
    Info { session: String, text: String },
    Error { session: String, error: String },
    TurnComplete { session: String, usage: Usage, aborted: bool },
}

impl Event {
    pub fn session(&self) -> &str {
        match self {
            Event::TurnStarted { session }
            | Event::TextDelta { session, .. }
            | Event::ThinkingDelta { session, .. }
            | Event::AssistantMessage { session, .. }
            | Event::ToolStart { session, .. }
            | Event::ToolEnd { session, .. }
            | Event::SubagentStarted { session, .. }
            | Event::SubagentFinished { session, .. }
            | Event::Compacted { session, .. }
            | Event::Retrying { session, .. }
            | Event::Info { session, .. }
            | Event::Error { session, .. }
            | Event::TurnComplete { session, .. } => session,
        }
    }
}
