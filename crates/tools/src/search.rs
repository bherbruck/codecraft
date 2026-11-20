use async_trait::async_trait;
use codecraft_protocol::ToolSpec;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks::UTF8, SearcherBuilder};
use serde_json::{json, Value};

use crate::truncate::clip_line;
use crate::{arg_bool, arg_str, arg_u64, Access, Tool, ToolCtx, ToolOutput};

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regex (ripgrep engine). Respects .gitignore. Returns \
                          `path:line: text`. Default limit 100 matches."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex (or literal text with literal=true)"},
                    "path": {"type": "string", "description": "File or directory (default: working directory)"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. `*.rs`"},
                    "ignore_case": {"type": "boolean"},
                    "literal": {"type": "boolean"},
                    "limit": {"type": "integer"}
                },
                "required": ["pattern"]
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
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let root = ctx.resolve(args["path"].as_str().unwrap_or("."));
        let glob = args["glob"].as_str().map(String::from);
        let ignore_case = arg_bool(&args, "ignore_case");
        let literal = arg_bool(&args, "literal");
        let limit = arg_u64(&args, "limit").unwrap_or(100) as usize;
        let cwd = ctx.cwd.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let pat = if literal { regex::escape(&pattern) } else { pattern };
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(ignore_case)
                .build(&pat)
                .map_err(|e| format!("bad regex: {e}"))?;
            let mut wb = ignore::WalkBuilder::new(&root);
            wb.hidden(false).filter_entry(|e| e.file_name() != ".git");
            if let Some(g) = &glob {
                let mut ob = ignore::overrides::OverrideBuilder::new(&root);
                ob.add(g).map_err(|e| format!("bad glob: {e}"))?;
                wb.overrides(ob.build().map_err(|e| e.to_string())?);
            }
            let mut searcher = SearcherBuilder::new().line_number(true).build();
            let mut out = Vec::new();
            let mut more = false;
            for entry in wb.build().flatten() {
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let p = entry.path().to_path_buf();
                let shown = p.strip_prefix(&cwd).unwrap_or(&p).display().to_string();
                let _ = searcher.search_path(
                    &matcher,
                    &p,
                    UTF8(|ln, line| {
                        if out.len() >= limit {
                            more = true;
                            return Ok(false);
                        }
                        out.push(format!("{shown}:{ln}: {}", clip_line(line.trim_end(), 500)));
                        Ok(true)
                    }),
                );
                if more {
                    break;
                }
            }
            if out.is_empty() {
                return Ok("No matches.".into());
            }
            let mut s = out.join("\n");
            if more {
                s.push_str(&format!("\n[limit of {limit} reached; narrow the search or raise limit]"));
            }
            Ok(s)
        })
        .await;
        match res {
            Ok(Ok(s)) => ToolOutput::ok(s),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. `**/*.rs`, `src/**/mod.rs`). Respects .gitignore. \
                          Default limit 1000."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search (default: working directory)"},
                    "limit": {"type": "integer"}
                },
                "required": ["pattern"]
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
        let pattern = match arg_str(&args, "pattern") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let root = ctx.resolve(args["path"].as_str().unwrap_or("."));
        let limit = arg_u64(&args, "limit").unwrap_or(1000) as usize;
        let cwd = ctx.cwd.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let pat = if pattern.contains('/') || pattern.starts_with("**") { pattern } else { format!("**/{pattern}") };
            let set = globset::GlobBuilder::new(&pat)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("bad glob: {e}"))?
                .compile_matcher();
            let mut out = Vec::new();
            let mut wb = ignore::WalkBuilder::new(&root);
            wb.hidden(false).filter_entry(|e| e.file_name() != ".git");
            for e in wb.build().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(&root).unwrap_or(p);
                if rel.as_os_str().is_empty() || !set.is_match(rel) {
                    continue;
                }
                out.push(p.strip_prefix(&cwd).unwrap_or(p).display().to_string());
                if out.len() > limit {
                    break;
                }
            }
            if out.is_empty() {
                return Ok("No files found.".into());
            }
            let more = out.len() > limit;
            out.truncate(limit);
            out.sort();
            let mut s = out.join("\n");
            if more {
                s.push_str(&format!("\n[limit of {limit} reached]"));
            }
            Ok(s)
        })
        .await;
        match res {
            Ok(Ok(s)) => ToolOutput::ok(s),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}
