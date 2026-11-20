//! Built-in tools and the `Tool` trait.

pub mod bash;
pub mod edit;
pub mod fs;
pub mod sandbox;
pub mod search;
pub mod truncate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use codecraft_protocol::ToolSpec;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Per-call context.
#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub call_id: String,
    pub file_locks: FileLocks,
    /// When set, `bash` runs inside this microVM sandbox.
    pub sandbox: Option<Arc<sandbox::Firecracker>>,
}

impl ToolCtx {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = if let Some(rest) = p.strip_prefix("~/") {
            std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)
        } else {
            PathBuf::from(p)
        };
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }
}

/// Serializes mutations of the same file across parallel tool calls.
#[derive(Clone, Default)]
pub struct FileLocks(Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>);

impl FileLocks {
    pub async fn lock(&self, path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let m = self.0.lock().await.entry(key).or_default().clone();
        m.lock_owned().await
    }
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true }
    }
}

/// What kind of access a call needs; drives approvals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Read,
    Write { summary: String, key: String },
    Exec { summary: String, key: String },
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Read-only tools may run in parallel with each other.
    fn parallel_safe(&self) -> bool {
        false
    }
    fn access(&self, args: &Value, ctx: &ToolCtx) -> Access;
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput;
}

pub fn builtin() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(fs::Read),
        Arc::new(fs::Write),
        Arc::new(edit::Edit),
        Arc::new(bash::Bash),
        Arc::new(search::Grep),
        Arc::new(search::Glob),
        Arc::new(fs::Ls),
    ]
}

/// The directory copied into the sandbox: the git root, else cwd.
pub fn sandbox_root(cwd: &Path) -> PathBuf {
    cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf).unwrap_or_else(|| cwd.to_path_buf())
}

pub(crate) fn arg_str<'a>(args: &'a Value, k: &str) -> Result<&'a str, String> {
    args.get(k).and_then(|v| v.as_str()).ok_or_else(|| format!("missing required string parameter `{k}`"))
}

pub(crate) fn arg_u64(args: &Value, k: &str) -> Option<u64> {
    args.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

pub(crate) fn arg_bool(args: &Value, k: &str) -> bool {
    args.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}
