//! Provider abstraction: one neutral request/stream shape, several wire adapters.

pub mod anthropic;
pub mod claude_bridge;
pub mod openai_chat;
mod sse;

use std::sync::Arc;

use async_trait::async_trait;
use codecraft_protocol::{AssistantMessage, Message, StreamEvent, ThinkingLevel, ToolSpec};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub thinking: ThinkingLevel,
}

/// Executes codecraft tools on behalf of providers that run their own agent
/// loop (the claude bridge).
#[async_trait]
pub trait ToolHost: Send + Sync {
    async fn call(&self, id: &str, name: &str, args: Value) -> (String, bool);
}

/// What one sampling call produced.
pub enum Output {
    /// A single assistant message. The engine runs any tool calls in it.
    Message(AssistantMessage),
    /// The provider ran its own loop and already executed tools through the
    /// `ToolHost`. These are the messages it produced, in order.
    Transcript(Vec<Message>),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    /// True if the provider executes tools itself through a `ToolHost`.
    fn runs_own_loop(&self) -> bool {
        false
    }
    async fn sample(
        &self,
        req: Request,
        events: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
        host: Option<Arc<dyn ToolHost>>,
    ) -> anyhow::Result<Output>;
}

/// Error classification shared by adapters.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body)
    }
}
impl std::error::Error for ApiError {}

impl ApiError {
    pub fn retryable(&self) -> bool {
        matches!(self.status, 408 | 409 | 429 | 500..=599)
    }
    pub fn is_context_overflow(&self) -> bool {
        let b = self.body.to_lowercase();
        b.contains("context_length_exceeded")
            || b.contains("prompt is too long")
            || b.contains("maximum context length")
            || b.contains("context window")
    }
}

/// Is this error worth retrying?
pub fn is_retryable(e: &anyhow::Error) -> bool {
    if let Some(a) = e.downcast_ref::<ApiError>() {
        return a.retryable();
    }
    if let Some(r) = e.downcast_ref::<reqwest::Error>() {
        return r.is_timeout() || r.is_connect() || r.is_request() || r.is_body();
    }
    false
}

pub fn is_context_overflow(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().map(|a| a.is_context_overflow()).unwrap_or(false)
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("codecraft/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}
