//! Claude via the user's own `claude` CLI (Claude Code), the same approach
//! opencode uses. codecraft never touches Claude credentials: the user logs
//! in with `claude auth login`, and we run `claude -p` in stream-json mode
//! with its built-in tools disabled. codecraft's tools are served back to it
//! over MCP by `codecraft mcp-serve`, which relays calls to this process over
//! a local Unix socket so approvals and the UI stay codecraft's.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codecraft_protocol::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{Output, Provider, Request, ToolHost};

pub const MCP_PREFIX: &str = "mcp__codecraft__";

/// Shared between the provider and the socket server task.
struct Shared {
    tools: Vec<ToolSpec>,
    host: Option<Arc<dyn ToolHost>>,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

pub struct ClaudeBridge {
    pub id: String,
    pub binary: String,
    shared: Arc<std::sync::Mutex<Shared>>,
    state: Mutex<State>,
    socket: PathBuf,
    cwd: PathBuf,
}

#[derive(Default)]
struct State {
    proc: Option<Proc>,
    claude_session: Option<String>,
    /// How many of the engine's messages claude has already seen.
    seen: usize,
    listening: bool,
    system: String,
    model: String,
}

impl ClaudeBridge {
    pub fn new(id: impl Into<String>, binary: impl Into<String>, cwd: PathBuf) -> Self {
        let socket = std::env::temp_dir().join(format!("codecraft-{}.sock", uuid::Uuid::new_v4().simple()));
        Self {
            id: id.into(),
            binary: binary.into(),
            shared: Arc::new(std::sync::Mutex::new(Shared { tools: vec![], host: None })),
            state: Mutex::new(State::default()),
            socket,
            cwd,
        }
    }

    fn listen(&self) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&self.socket);
        let listener = UnixListener::bind(&self.socket).context("bind bridge socket")?;
        let shared = self.shared.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve_conn(stream, shared).await;
                });
            }
        });
        Ok(())
    }

    async fn spawn(&self, st: &mut State) -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let mcp = json!({"mcpServers": {"codecraft": {
            "type": "stdio",
            "command": exe.to_string_lossy(),
            "args": ["mcp-serve", "--socket", self.socket.to_string_lossy()],
        }}});
        let mut cmd = Command::new(&self.binary);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            &format!("{MCP_PREFIX}*"),
        ])
        .arg("--mcp-config")
        .arg(mcp.to_string())
        .arg("--append-system-prompt")
        .arg(&st.system);
        if !st.model.is_empty() && st.model != "default" {
            cmd.arg("--model").arg(&st.model);
        }
        if let Some(sid) = &st.claude_session {
            cmd.arg("--resume").arg(sid);
        }
        cmd.current_dir(&self.cwd)
            // codecraft loads AGENTS.md / CLAUDE.md itself.
            .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
            // Approvals can take a while; don't let claude time the tool out.
            .env("MCP_TOOL_TIMEOUT", "1800000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| {
            format!("could not start `{}`. Install Claude Code and run `claude auth login`.", self.binary)
        })?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        st.proc = Some(Proc { child, stdin, stdout });
        Ok(())
    }
}

async fn serve_conn(stream: UnixStream, shared: Arc<std::sync::Mutex<Shared>>) -> anyhow::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let req: Value = serde_json::from_str(&line)?;
        let resp = match req["op"].as_str() {
            Some("list") => {
                let tools = shared.lock().unwrap().tools.clone();
                json!({"tools": tools})
            }
            Some("call") => {
                let host = shared.lock().unwrap().host.clone();
                let name = req["name"].as_str().unwrap_or("").to_string();
                let id = format!("cc_{}", uuid::Uuid::new_v4().simple());
                match host {
                    Some(h) => {
                        let (output, is_error) = h.call(&id, &name, req["args"].clone()).await;
                        json!({"output": output, "is_error": is_error})
                    }
                    None => json!({"output": "codecraft is not running a turn", "is_error": true}),
                }
            }
            _ => json!({"error": "unknown op"}),
        };
        w.write_all(format!("{resp}\n").as_bytes()).await?;
    }
    Ok(())
}

/// Render messages claude hasn't seen as one user message.
fn render_unseen(msgs: &[Message]) -> String {
    if msgs.iter().all(|m| matches!(m, Message::User { .. })) {
        return msgs
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content.clone(),
                _ => vec![],
            })
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let mut s = String::from(
        "<conversation-checkpoint>\nThe conversation so far happened with a different model or was compacted. Transcript:\n",
    );
    for m in msgs {
        match m {
            Message::User { content } => {
                for b in content {
                    if let ContentBlock::Text { text } = b {
                        s.push_str(&format!("\n[user]\n{text}\n"));
                    }
                }
            }
            Message::Assistant(a) => {
                let t = a.text();
                if !t.is_empty() {
                    s.push_str(&format!("\n[assistant]\n{t}\n"));
                }
                for (_, name, args) in a.tool_calls() {
                    s.push_str(&format!("\n[tool call {name}] {args}\n"));
                }
            }
            Message::ToolResult { tool_name, content, .. } => {
                for b in content {
                    if let ContentBlock::Text { text } = b {
                        let short: String = text.chars().take(2000).collect();
                        s.push_str(&format!("\n[tool result {tool_name}]\n{short}\n"));
                    }
                }
            }
        }
    }
    s.push_str("\n</conversation-checkpoint>\nContinue from the last user message.");
    s
}

fn strip(name: &str) -> String {
    name.strip_prefix(MCP_PREFIX).unwrap_or(name).to_string()
}

#[async_trait]
impl Provider for ClaudeBridge {
    fn id(&self) -> &str {
        &self.id
    }
    fn runs_own_loop(&self) -> bool {
        true
    }

    async fn sample(
        &self,
        req: Request,
        events: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
        host: Option<Arc<dyn ToolHost>>,
    ) -> anyhow::Result<Output> {
        {
            let mut sh = self.shared.lock().unwrap();
            sh.tools = req.tools.clone();
            sh.host = host;
        }
        let mut st = self.state.lock().await;
        if !st.listening {
            self.listen()?;
            st.listening = true;
        }
        // Model or history change we can't express incrementally → new process.
        let model_changed = st.model != req.model;
        if st.seen > req.messages.len() || model_changed {
            if let Some(mut p) = st.proc.take() {
                let _ = p.child.kill().await;
            }
            if st.seen > req.messages.len() {
                st.claude_session = None;
                st.seen = 0;
            }
        }
        st.model = req.model.clone();
        st.system = req.system.clone();
        if st.proc.is_none() {
            self.spawn(&mut st).await?;
        }
        let unseen = render_unseen(&req.messages[st.seen..]);
        let input = json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": unseen}]},
            "parent_tool_use_id": null,
            "session_id": st.claude_session.clone().unwrap_or_default(),
        });
        {
            let p = st.proc.as_mut().unwrap();
            p.stdin.write_all(format!("{input}\n").as_bytes()).await?;
            p.stdin.flush().await?;
        }

        let mut transcript: Vec<Message> = Vec::new();
        let mut cur: Option<(String, AssistantMessage)> = None;
        let mut result: Option<Value> = None;
        let mut aborted = false;
        loop {
            let line = {
                let p = st.proc.as_mut().unwrap();
                tokio::select! {
                    _ = cancel.cancelled() => { aborted = true; break; }
                    l = p.stdout.next_line() => l?,
                }
            };
            let Some(line) = line else {
                st.proc = None;
                bail!("claude exited unexpectedly (is it logged in? try `claude auth login`)");
            };
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or("") {
                "system" => {
                    if let Some(sid) = v["session_id"].as_str() {
                        st.claude_session = Some(sid.to_string());
                    }
                }
                "stream_event" => {
                    let e = &v["event"];
                    if v["parent_tool_use_id"].is_string() {
                        continue;
                    }
                    if e["type"] == "content_block_delta" {
                        let d = &e["delta"];
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => {
                                let _ = events.send(StreamEvent::TextDelta(d["text"].as_str().unwrap_or("").into()));
                            }
                            "thinking_delta" => {
                                let _ = events
                                    .send(StreamEvent::ThinkingDelta(d["thinking"].as_str().unwrap_or("").into()));
                            }
                            _ => {}
                        }
                    }
                }
                "assistant" => {
                    let m = &v["message"];
                    let mid = m["id"].as_str().unwrap_or("").to_string();
                    let entry = match &mut cur {
                        Some((id, _)) if *id == mid => &mut cur.as_mut().unwrap().1,
                        _ => {
                            if let Some((_, a)) = cur.take() {
                                transcript.push(Message::Assistant(a));
                            }
                            cur = Some((
                                mid,
                                AssistantMessage {
                                    model: m["model"].as_str().unwrap_or(&req.model).into(),
                                    ..Default::default()
                                },
                            ));
                            &mut cur.as_mut().unwrap().1
                        }
                    };
                    for b in m["content"].as_array().into_iter().flatten() {
                        match b["type"].as_str().unwrap_or("") {
                            "text" => entry.content.push(ContentBlock::text(b["text"].as_str().unwrap_or(""))),
                            "thinking" => entry.content.push(ContentBlock::Thinking {
                                thinking: b["thinking"].as_str().unwrap_or("").into(),
                                signature: None,
                            }),
                            "tool_use" => {
                                entry.stop_reason = StopReason::ToolUse;
                                entry.content.push(ContentBlock::ToolCall {
                                    id: b["id"].as_str().unwrap_or("").into(),
                                    name: strip(b["name"].as_str().unwrap_or("")),
                                    arguments: b["input"].clone(),
                                })
                            }
                            _ => {}
                        }
                    }
                }
                "user" => {
                    if let Some((_, a)) = cur.take() {
                        transcript.push(Message::Assistant(a));
                    }
                    for b in v["message"]["content"].as_array().into_iter().flatten() {
                        if b["type"] == "tool_result" {
                            let text = match &b["content"] {
                                Value::String(s) => s.clone(),
                                Value::Array(a) => a
                                    .iter()
                                    .filter_map(|x| x["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            let id = b["tool_use_id"].as_str().unwrap_or("").to_string();
                            let tool_name = transcript
                                .iter()
                                .rev()
                                .find_map(|m| match m {
                                    Message::Assistant(a) => {
                                        a.tool_calls().find(|(i, _, _)| *i == id).map(|(_, n, _)| n.to_string())
                                    }
                                    _ => None,
                                })
                                .unwrap_or_default();
                            transcript.push(Message::ToolResult {
                                tool_call_id: id,
                                tool_name,
                                content: vec![ContentBlock::text(text)],
                                is_error: b["is_error"].as_bool().unwrap_or(false),
                            });
                        }
                    }
                }
                "result" => {
                    result = Some(v);
                    break;
                }
                _ => {}
            }
        }
        if let Some((_, a)) = cur.take() {
            transcript.push(Message::Assistant(a));
        }
        if aborted {
            // Kill and resume next time; claude keeps its own transcript.
            if let Some(mut p) = st.proc.take() {
                let _ = p.child.kill().await;
            }
            match transcript.iter_mut().rev().find_map(|m| match m {
                Message::Assistant(a) => Some(a),
                _ => None,
            }) {
                Some(a) => a.stop_reason = StopReason::Aborted,
                None => transcript.push(Message::Assistant(AssistantMessage {
                    stop_reason: StopReason::Aborted,
                    model: req.model.clone(),
                    ..Default::default()
                })),
            }
        }
        if let Some(r) = &result {
            let u = &r["usage"];
            let usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                // Subscription: no per-token billing to report.
                cost: 0.0,
            };
            if let Some(Message::Assistant(a)) = transcript.iter_mut().rev().find(|m| matches!(m, Message::Assistant(_)))
            {
                a.usage = usage;
            }
            if r["is_error"].as_bool() == Some(true) {
                let msg = r["result"].as_str().map(String::from).unwrap_or_else(|| r.to_string());
                if transcript.is_empty() {
                    return Err(anyhow!("claude: {msg}"));
                }
            }
        }
        // Don't keep the turn's tool host (and its event sender) alive between turns.
        self.shared.lock().unwrap().host = None;
        // Everything up to now (sent + produced) is in claude's own history.
        st.seen = req.messages.len() + transcript.len();
        if !transcript.iter().any(|m| matches!(m, Message::Assistant(_))) {
            transcript.push(Message::Assistant(AssistantMessage { model: req.model.clone(), ..Default::default() }));
        }
        Ok(Output::Transcript(transcript))
    }
}

impl Drop for ClaudeBridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `codecraft mcp-serve --socket PATH`: a minimal MCP stdio server that relays
/// tool listing and calls to the codecraft process owning the socket.
pub async fn mcp_serve(socket: PathBuf) -> anyhow::Result<()> {
    let stream = UnixStream::connect(&socket).await.context("connect to codecraft")?;
    let (r, mut w) = stream.into_split();
    let mut sock_lines = BufReader::new(r).lines();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = stdin.next_line().await? {
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = req.get("id").cloned() else { continue }; // notification
        let method = req["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "codecraft", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => {
                w.write_all(b"{\"op\":\"list\"}\n").await?;
                let resp: Value = serde_json::from_str(&sock_lines.next_line().await?.unwrap_or_default())?;
                let tools: Vec<Value> = resp["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| json!({"name": t["name"], "description": t["description"], "inputSchema": t["parameters"]}))
                    .collect();
                Ok(json!({"tools": tools}))
            }
            "tools/call" => {
                let call = json!({"op":"call","name":req["params"]["name"],"args":req["params"]["arguments"]});
                w.write_all(format!("{call}\n").as_bytes()).await?;
                let resp: Value = serde_json::from_str(&sock_lines.next_line().await?.unwrap_or_default())?;
                Ok(json!({
                    "content": [{"type": "text", "text": resp["output"].as_str().unwrap_or("")}],
                    "isError": resp["is_error"].as_bool().unwrap_or(false),
                }))
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let msg = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e}),
        };
        stdout.write_all(format!("{msg}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}
