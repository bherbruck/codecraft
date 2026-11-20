use async_trait::async_trait;
use codecraft_protocol::ToolSpec;
use serde_json::{json, Value};

use crate::truncate::{clip_line, MAX_BYTES, MAX_LINES};
use crate::{arg_str, arg_u64, Access, Tool, ToolCtx, ToolOutput};

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: format!(
                "Read a text file. Output has line numbers (`N\\t...`). Reads at most {MAX_LINES} lines / {} KB; \
                 use `offset` (1-based line) and `limit` for more. Lines over 2000 chars are clipped.",
                MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "offset": {"type": "integer", "description": "1-based line to start at"},
                    "limit": {"type": "integer", "description": "Max lines to read"}
                },
                "required": ["path"]
            }),
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn access(&self, _: &Value, _: &ToolCtx) -> Access {
        Access::Read
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let path = match arg_str(&args, "path") {
            Ok(p) => ctx.resolve(p),
            Err(e) => return ToolOutput::err(e),
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        if bytes.iter().take(8000).any(|b| *b == 0) {
            return ToolOutput::err(format!("{} looks like a binary file ({} bytes)", path.display(), bytes.len()));
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = arg_u64(&args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_u64(&args, "limit").map(|l| l as usize).unwrap_or(MAX_LINES).min(MAX_LINES);
        let total = text.lines().count();
        if total == 0 {
            return ToolOutput::ok("(empty file)");
        }
        if offset > total {
            return ToolOutput::err(format!("offset {offset} is past the end of the file ({total} lines)"));
        }
        let mut out = String::new();
        let mut last = offset - 1;
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let l = format!("{:>6}\t{}\n", i + 1, clip_line(line, 2000));
            if out.len() + l.len() > MAX_BYTES {
                break;
            }
            out.push_str(&l);
            last = i + 1;
        }
        if last < total {
            out.push_str(&format!(
                "\n[Showing lines {offset}-{last} of {total}. Use offset={} to continue.]",
                last + 1
            ));
        }
        ToolOutput::ok(out)
    }
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create or overwrite a file with the given content. Creates parent directories. \
                          Prefer `edit` for changes to existing files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }
    fn access(&self, args: &Value, ctx: &ToolCtx) -> Access {
        let p = ctx.resolve(args["path"].as_str().unwrap_or(""));
        Access::Write { summary: format!("write {}", p.display()), key: "write".into() }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let (path, content) = match (arg_str(&args, "path"), arg_str(&args, "content")) {
            (Ok(p), Ok(c)) => (ctx.resolve(p), c.to_string()),
            (Err(e), _) | (_, Err(e)) => return ToolOutput::err(e),
        };
        let _g = ctx.file_locks.lock(&path).await;
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::err(format!("cannot create {}: {e}", parent.display()));
            }
        }
        let existed = path.exists();
        match tokio::fs::write(&path, &content).await {
            Ok(_) => ToolOutput::ok(format!(
                "{} {} ({} lines)",
                if existed { "Overwrote" } else { "Created" },
                path.display(),
                content.lines().count()
            )),
            Err(e) => ToolOutput::err(format!("cannot write {}: {e}", path.display())),
        }
    }
}

pub struct Ls;

#[async_trait]
impl Tool for Ls {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ls".into(),
            description: "List a directory (non-recursive). Directories end with `/`.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Defaults to the working directory"}},
            }),
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn access(&self, _: &Value, _: &ToolCtx) -> Access {
        Access::Read
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let path = ctx.resolve(args["path"].as_str().unwrap_or("."));
        let mut rd = match tokio::fs::read_dir(&path).await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("cannot list {}: {e}", path.display())),
        };
        let mut names = Vec::new();
        while let Ok(Some(e)) = rd.next_entry().await {
            let mut n = e.file_name().to_string_lossy().to_string();
            if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                n.push('/');
            }
            names.push(n);
        }
        names.sort();
        let total = names.len();
        names.truncate(500);
        let mut out = names.join("\n");
        if total > 500 {
            out.push_str(&format!("\n[{} more entries not shown]", total - 500));
        }
        if out.is_empty() {
            out = "(empty directory)".into();
        }
        ToolOutput::ok(out)
    }
}
