use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use codecraft_protocol::ToolSpec;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::truncate::{head_tail, MAX_BYTES, MAX_LINES};
use crate::{arg_str, arg_u64, Access, Tool, ToolCtx, ToolOutput};

pub struct Bash;

const DEFAULT_TIMEOUT: u64 = 300;
const MAX_TIMEOUT: u64 = 3600;

const SAFE: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "pwd", "echo", "which", "file", "stat", "tree", "du", "df",
    "date", "whoami", "uname", "env", "printenv", "basename", "dirname", "realpath", "sort", "uniq", "cut", "tr",
    "diff", "true", "false", "type", "nl", "less", "more", "jq",
];
const SAFE_GIT: &[&str] = &["status", "log", "diff", "show", "branch", "rev-parse", "ls-files", "blame", "remote", "describe"];

/// Is this command known to be read-only? Conservative.
pub fn is_known_safe(cmd: &str) -> bool {
    if cmd.contains('>') || cmd.contains("$(") || cmd.contains('`') || cmd.contains('\n') {
        return false;
    }
    let segments = cmd.split(|c| c == ';' || c == '|' || c == '&').map(str::trim).filter(|s| !s.is_empty());
    let mut any = false;
    for seg in segments {
        any = true;
        let mut words = seg.split_whitespace();
        let Some(first) = words.next() else { return false };
        let ok = match first {
            "git" => words.next().map(|w| SAFE_GIT.contains(&w)).unwrap_or(false),
            "find" => !seg.contains("-exec") && !seg.contains("-delete") && !seg.contains("-ok"),
            "sed" => seg.contains("-n") && !seg.contains("-i"),
            "cargo" => matches!(words.next(), Some("check") | Some("tree") | Some("metadata")),
            w => SAFE.contains(&w),
        };
        if !ok {
            return false;
        }
    }
    any
}

fn first_word(cmd: &str) -> String {
    cmd.split_whitespace().next().unwrap_or("").to_string()
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: format!(
                "Run a shell command with `bash -c` in the working directory. stdout and stderr are combined. \
                 Output over {MAX_LINES} lines / {} KB keeps the head and tail; the full output is saved to a \
                 temp file whose path is returned. Default timeout {DEFAULT_TIMEOUT}s. Don't use it for \
                 interactive programs.",
                MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "Seconds (default 300, max 3600)"},
                    "workdir": {"type": "string", "description": "Directory to run in (default: working directory)"}
                },
                "required": ["command"]
            }),
        }
    }
    fn access(&self, args: &Value, ctx: &ToolCtx) -> Access {
        let cmd = args["command"].as_str().unwrap_or("");
        if is_known_safe(cmd) {
            Access::Read
        } else if ctx.sandbox.is_some() {
            // Sandboxed: no network, writes confined to the workspace. Treat like a file edit.
            Access::Write { summary: format!("[sandbox] {cmd}"), key: "write".into() }
        } else {
            Access::Exec { summary: cmd.to_string(), key: format!("bash:{}", first_word(cmd)) }
        }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let cmd = match arg_str(&args, "command") {
            Ok(c) => c.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let timeout = arg_u64(&args, "timeout").unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
        let dir = args["workdir"].as_str().map(|d| ctx.resolve(d)).unwrap_or_else(|| ctx.cwd.clone());
        if let Some(sb) = &ctx.sandbox {
            return run_sandboxed(sb, ctx, &dir, &cmd, timeout).await;
        }

        let mut c = Command::new("bash");
        c.arg("-c")
            .arg(&cmd)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CODECRAFT", "1")
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .kill_on_drop(true);
        #[cfg(unix)]
        c.process_group(0);
        let mut child = match c.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(format!("failed to start bash: {e}")),
        };
        let pid = child.id();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut readers = Vec::new();
        if let Some(mut out) = child.stdout.take() {
            let b = buf.clone();
            readers.push(tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = out.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    b.lock().await.extend_from_slice(&chunk[..n]);
                }
            }));
        }
        if let Some(mut err) = child.stderr.take() {
            let b = buf.clone();
            readers.push(tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = err.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    b.lock().await.extend_from_slice(&chunk[..n]);
                }
            }));
        }

        let kill_group = || {
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
        };
        let mut note = String::new();
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                kill_group();
                note = format!("\n[killed: timed out after {timeout}s]");
                None
            }
            _ = ctx.cancel.cancelled() => {
                kill_group();
                note = "\n[killed: interrupted by user]".into();
                None
            }
        };
        // Background children may hold the pipes open; don't wait forever.
        for r in readers {
            let _ = tokio::time::timeout(Duration::from_millis(500), r).await;
        }
        let raw = buf.lock().await.clone();
        let text = String::from_utf8_lossy(&raw).to_string();
        let t = head_tail(&text, MAX_LINES, MAX_BYTES);
        let mut out = t.text;
        if t.truncated {
            let path = std::env::temp_dir().join(format!("codecraft-bash-{}.log", uuid::Uuid::new_v4().simple()));
            let _ = std::fs::write(&path, &raw);
            out = format!(
                "[output truncated: {} lines total; full output in {}]\n{out}",
                t.total_lines,
                path.display()
            );
        }
        if out.trim().is_empty() {
            out = "(no output)".into();
        }
        out.push_str(&note);
        match status {
            Some(s) if s.success() => ToolOutput::ok(out),
            Some(s) => {
                let code = s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                ToolOutput::err(format!("{out}\n[exit code {code}]"))
            }
            None => ToolOutput::err(out),
        }
    }
}

async fn run_sandboxed(
    sb: &crate::sandbox::Firecracker,
    ctx: &ToolCtx,
    dir: &std::path::Path,
    cmd: &str,
    timeout: u64,
) -> ToolOutput {
    let root = crate::sandbox_root(&ctx.cwd);
    let Ok(sub) = dir.strip_prefix(&root) else {
        return ToolOutput::err(format!("sandboxed commands must run inside the workspace {}", root.display()));
    };
    match sb.run(&root, &sub.to_string_lossy(), cmd, Duration::from_secs(timeout), &ctx.cancel).await {
        Ok(r) => {
            let t = head_tail(&r.output, MAX_LINES, MAX_BYTES);
            let mut out = if t.text.trim().is_empty() { "(no output)".to_string() } else { t.text };
            if !r.changed.is_empty() || !r.deleted.is_empty() {
                out.push_str(&format!(
                    "\n[sandbox: {} file(s) changed, {} deleted]",
                    r.changed.len(),
                    r.deleted.len()
                ));
            }
            match r.exit_code {
                Some(0) => ToolOutput::ok(out),
                Some(c) => ToolOutput::err(format!("{out}\n[exit code {c}]")),
                None => ToolOutput::err(out),
            }
        }
        Err(e) => ToolOutput::err(format!("sandbox error: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::is_known_safe;
    #[test]
    fn safety() {
        assert!(is_known_safe("ls -la"));
        assert!(is_known_safe("git status && git diff"));
        assert!(is_known_safe("cat a | grep b"));
        assert!(!is_known_safe("rm -rf /"));
        assert!(!is_known_safe("echo hi > f"));
        assert!(!is_known_safe("find . -delete"));
        assert!(!is_known_safe("git push"));
    }
}
