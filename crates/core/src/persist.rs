//! Persistence hook. `codecraft-store` implements this with SQLite.

use codecraft_protocol::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub parent_id: Option<String>,
    pub cwd: String,
    pub model: String,
    pub agent: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Message { message: Message },
    /// Messages before this entry are replaced by `summary` when rebuilding context.
    Compaction { summary: String, tokens_before: u64 },
    ModelChange { model: String },
}

pub trait Persist: Send + Sync {
    fn create_session(&self, meta: &SessionMeta) -> anyhow::Result<()>;
    fn append(&self, session_id: &str, entry: &Entry) -> anyhow::Result<()>;
}

/// Rebuild the model-visible message list from stored entries.
pub fn rebuild(entries: &[Entry]) -> Vec<Message> {
    let mut msgs = Vec::new();
    for e in entries {
        match e {
            Entry::Message { message } => msgs.push(message.clone()),
            Entry::Compaction { summary, .. } => {
                // Compaction keeps a suffix of recent messages; the entry records
                // the summary and is followed by re-appended kept messages.
                msgs.clear();
                msgs.push(crate::compact::summary_message(summary));
            }
            Entry::ModelChange { .. } => {}
        }
    }
    msgs
}
