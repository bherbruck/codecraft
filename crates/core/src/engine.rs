//! Sessions and the agent loop.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use codecraft_llm::{is_context_overflow, is_retryable, Output, Provider, Request, ToolHost};
use codecraft_protocol::*;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agents::AgentDef;
use crate::approval::Approvals;
use crate::compact;
use crate::config::Config;
use crate::persist::{Entry, Persist, SessionMeta};
use crate::prompt::{self, PromptParts};
use crate::runner::ToolRunner;
use crate::task::TaskTool;

const MAX_RETRIES: u32 = 3;
const RESERVE_TOKENS: u64 = 16_384;

type Queue = Arc<std::sync::Mutex<VecDeque<String>>>;

/// Everything needed to start a session.
#[derive(Clone)]
pub struct SessionSetup {
    pub config: Arc<Config>,
    pub cwd: PathBuf,
    /// `provider/model`
    pub model: String,
    pub approvals: Arc<Approvals>,
    pub events: UnboundedSender<Event>,
    pub persist: Option<Arc<dyn Persist>>,
    pub agents: Vec<AgentDef>,
    /// Agent definition this session runs as (None = main agent).
    pub agent: Option<AgentDef>,
    pub depth: u32,
    pub parent_id: Option<String>,
    /// Resume an existing session.
    pub id: Option<String>,
    pub messages: Vec<Message>,
}

pub struct Session {
    pub id: String,
    pub setup: SessionSetup,
    pub messages: Vec<Message>,
    pub usage: Usage,
    provider: Arc<dyn Provider>,
    model_id: String,
    runner: Arc<ToolRunner>,
    system: String,
    persisted: bool,
    steer: Queue,
    follow: Queue,
}

impl Session {
    pub fn new(setup: SessionSetup) -> anyhow::Result<Session> {
        let id = setup.id.clone().unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let (provider, model_id) = setup.config.provider(&setup.model, &setup.cwd)?;

        let mut tools = codecraft_tools::builtin();
        if setup.depth < setup.config.subagent_depth {
            tools.push(Arc::new(TaskTool::new(setup.clone(), id.clone())));
        }
        if let Some(allowed) = setup.agent.as_ref().and_then(|a| a.tools.clone()) {
            tools.retain(|t| allowed.contains(&t.spec().name));
        }
        let mut runner = ToolRunner::new(id.clone(), tools, setup.cwd.clone(), setup.approvals.clone(), setup.events.clone());
        let sandboxed = match setup.config.sandbox.as_str() {
            "" | "none" | "off" => false,
            "firecracker" => {
                let dir = crate::config::home_dir().join("sandbox");
                let fc = codecraft_tools::sandbox::Firecracker::from_dir(&dir).ok_or_else(|| {
                    anyhow::anyhow!("firecracker sandbox not installed (or no /dev/kvm). Run `codecraft sandbox setup`.")
                })?;
                runner.sandbox = Some(Arc::new(fc));
                true
            }
            other => anyhow::bail!("unknown sandbox `{other}` (use none or firecracker)"),
        };
        let runner = Arc::new(runner);
        let specs = runner.specs();
        let agent_list: Vec<(String, String)> = if specs.iter().any(|s| s.name == "task") {
            setup.agents.iter().map(|a| (a.name.clone(), a.description.clone())).collect()
        } else {
            vec![]
        };
        let system = prompt::build(PromptParts {
            cwd: &setup.cwd,
            tools: &specs,
            agent_prompt: setup.agent.as_ref().map(|a| a.prompt.as_str()),
            agents: &agent_list,
        });
        let system = if sandboxed {
            system + "\n<sandbox>\n`bash` runs inside an isolated Firecracker microVM (Ubuntu) with NO network access. \
                      The workspace is copied in and changed files are synced back after each command. Package \
                      installs and downloads will fail.\n</sandbox>\n"
        } else {
            system
        };
        let persisted = setup.id.is_some();
        let messages = setup.messages.clone();
        Ok(Session {
            id,
            setup,
            messages,
            usage: Usage::default(),
            provider,
            model_id,
            runner,
            system,
            persisted,
            steer: Default::default(),
            follow: Default::default(),
        })
    }

    pub fn model(&self) -> &str {
        &self.setup.model
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.runner.order.clone()
    }

    pub fn set_model(&mut self, spec: &str) -> anyhow::Result<()> {
        let (p, m) = self.setup.config.provider(spec, &self.setup.cwd)?;
        self.provider = p;
        self.model_id = m;
        self.setup.model = spec.to_string();
        self.record(Entry::ModelChange { model: spec.to_string() });
        Ok(())
    }

    fn emit(&self, e: Event) {
        let _ = self.setup.events.send(e);
    }

    fn record(&mut self, entry: Entry) {
        let Some(p) = self.setup.persist.clone() else { return };
        if !self.persisted {
            let title = self.messages.iter().find_map(|m| match m {
                Message::User { content } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.chars().take(80).collect::<String>()),
                    _ => None,
                }),
                _ => None,
            });
            let meta = SessionMeta {
                id: self.id.clone(),
                parent_id: self.setup.parent_id.clone(),
                cwd: self.setup.cwd.display().to_string(),
                model: self.setup.model.clone(),
                agent: self.setup.agent.as_ref().map(|a| a.name.clone()),
                title,
            };
            if let Err(e) = p.create_session(&meta) {
                tracing::warn!("persist create: {e}");
            }
            self.persisted = true;
        }
        if let Err(e) = p.append(&self.id, &entry) {
            tracing::warn!("persist append: {e}");
        }
    }

    fn push(&mut self, m: Message) {
        self.messages.push(m.clone());
        self.record(Entry::Message { message: m });
    }

    fn context_estimate(&self) -> u64 {
        let last = self.messages.iter().rposition(|m| matches!(m, Message::Assistant(a) if a.usage.context_tokens() > 0));
        match last {
            Some(i) => {
                let Message::Assistant(a) = &self.messages[i] else { unreachable!() };
                a.usage.context_tokens() + self.messages[i + 1..].iter().map(|m| m.approx_tokens()).sum::<u64>()
            }
            None => self.messages.iter().map(|m| m.approx_tokens()).sum::<u64>() + (self.system.len() as u64) / 4,
        }
    }

    fn should_compact(&self) -> bool {
        if self.provider.runs_own_loop() {
            return false; // claude manages its own context
        }
        let window = self.setup.config.context_window(&self.setup.model);
        let limit = (window * 9 / 10).min(window.saturating_sub(RESERVE_TOKENS));
        self.context_estimate() > limit
    }

    /// Summarize older history. Returns false if there was nothing to compact.
    pub async fn compact(&mut self, cancel: CancellationToken) -> anyhow::Result<bool> {
        let Some(cut) = compact::cut_point(&self.messages, compact::KEEP_RECENT_TOKENS) else {
            return Ok(false);
        };
        let before = self.context_estimate();
        let summary = compact::summarize(&self.provider, &self.model_id, &self.messages[..cut], cancel).await?;
        if summary.trim().is_empty() {
            anyhow::bail!("compaction produced an empty summary");
        }
        let kept: Vec<Message> = self.messages[cut..].to_vec();
        self.messages = vec![compact::summary_message(&summary)];
        self.record(Entry::Compaction { summary, tokens_before: before });
        for m in kept {
            self.push(m);
        }
        self.emit(Event::Compacted { session: self.id.clone(), tokens_before: before });
        Ok(true)
    }

    async fn sample_once(&self, cancel: &CancellationToken) -> anyhow::Result<Output> {
        let req = Request {
            model: self.model_id.clone(),
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.runner.specs(),
            max_tokens: self.setup.config.max_tokens,
            thinking: self.setup.config.thinking,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        let events = self.setup.events.clone();
        let sid = self.id.clone();
        let fwd = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let e = match ev {
                    StreamEvent::TextDelta(text) => Event::TextDelta { session: sid.clone(), text },
                    StreamEvent::ThinkingDelta(text) => Event::ThinkingDelta { session: sid.clone(), text },
                    _ => continue,
                };
                let _ = events.send(e);
            }
        });
        let host: Option<Arc<dyn ToolHost>> =
            if self.provider.runs_own_loop() { Some(self.runner.clone() as Arc<dyn ToolHost>) } else { None };
        let out = self.provider.sample(req, tx, cancel.clone(), host).await;
        let _ = fwd.await;
        out
    }

    async fn sample(&mut self, cancel: &CancellationToken) -> anyhow::Result<Output> {
        let mut attempt = 0;
        let mut compacted = false;
        loop {
            match self.sample_once(cancel).await {
                Ok(o) => return Ok(o),
                Err(e) if cancel.is_cancelled() => return Err(e),
                Err(e) if is_context_overflow(&e) && !compacted => {
                    compacted = true;
                    if !self.compact(cancel.clone()).await? {
                        return Err(e);
                    }
                }
                Err(e) if is_retryable(&e) && attempt < MAX_RETRIES => {
                    attempt += 1;
                    self.emit(Event::Retrying { session: self.id.clone(), attempt, error: e.to_string() });
                    let delay = Duration::from_millis(2000 * 2u64.pow(attempt - 1));
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn take_queued(&self, steer_only: bool) -> Vec<String> {
        let mut out: Vec<String> = self.steer.lock().unwrap().drain(..).collect();
        if out.is_empty() && !steer_only {
            if let Some(f) = self.follow.lock().unwrap().pop_front() {
                out.push(f);
            }
        }
        out
    }

    /// Run one user prompt to completion.
    pub async fn run(&mut self, input: &str, cancel: CancellationToken) -> anyhow::Result<()> {
        *self.runner.cancel.lock().unwrap() = cancel.clone();
        self.push(Message::user(input));
        self.emit(Event::TurnStarted { session: self.id.clone() });
        let mut turn_usage = Usage::default();
        let mut aborted = false;
        let mut length_failures = 0;
        let result: anyhow::Result<()> = async {
            loop {
                if self.should_compact() {
                    self.compact(cancel.clone()).await.context("auto-compaction failed")?;
                }
                let out = match self.sample(&cancel).await {
                    Ok(o) => o,
                    Err(_) if cancel.is_cancelled() => {
                        aborted = true;
                        break;
                    }
                    Err(e) => return Err(e),
                };
                match out {
                    Output::Message(a) => {
                        turn_usage.add(&a.usage);
                        let calls: Vec<(String, String, serde_json::Value)> =
                            a.tool_calls().map(|(i, n, v)| (i.into(), n.into(), v.clone())).collect();
                        let stop = a.stop_reason;
                        self.emit(Event::AssistantMessage { session: self.id.clone(), message: a.clone() });
                        self.push(Message::Assistant(a));
                        if stop == StopReason::Aborted || cancel.is_cancelled() {
                            for (id, name, _) in &calls {
                                self.push(tool_error(id, name, "aborted by user"));
                            }
                            aborted = true;
                            break;
                        }
                        if stop == StopReason::Length && !calls.is_empty() {
                            // Arguments may be cut off: never execute them.
                            for (id, name, _) in &calls {
                                self.push(tool_error(
                                    id,
                                    name,
                                    "Your output hit the max token limit, so this call's arguments may be truncated \
                                     and it was NOT run. Retry with smaller steps (e.g. several smaller edits).",
                                ));
                            }
                            length_failures += 1;
                            if length_failures > 2 {
                                anyhow::bail!("model kept hitting the output token limit");
                            }
                            continue;
                        }
                        if calls.is_empty() {
                            let q = self.take_queued(false);
                            if q.is_empty() {
                                break;
                            }
                            for t in q {
                                self.push(Message::user(t));
                            }
                            continue;
                        }
                        let runner = self.runner.clone();
                        let futs = calls.iter().map(|(id, name, args)| {
                            let r = runner.clone();
                            let (id, name, args) = (id.clone(), name.clone(), args.clone());
                            async move { r.execute(&id, &name, args).await }
                        });
                        let results = futures::future::join_all(futs).await;
                        for ((id, name, _), out) in calls.iter().zip(results) {
                            self.push(Message::ToolResult {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                content: vec![ContentBlock::text(out.content)],
                                is_error: out.is_error,
                            });
                        }
                        if cancel.is_cancelled() {
                            aborted = true;
                            break;
                        }
                        for t in self.take_queued(true) {
                            self.push(Message::user(t));
                        }
                    }
                    Output::Transcript(msgs) => {
                        let mut was_aborted = false;
                        for m in msgs {
                            if let Message::Assistant(a) = &m {
                                turn_usage.add(&a.usage);
                                was_aborted |= a.stop_reason == StopReason::Aborted;
                                self.emit(Event::AssistantMessage { session: self.id.clone(), message: a.clone() });
                            }
                            self.push(m);
                        }
                        if was_aborted || cancel.is_cancelled() {
                            aborted = true;
                            break;
                        }
                        let q = self.take_queued(false);
                        if q.is_empty() {
                            break;
                        }
                        for t in q {
                            self.push(Message::user(t));
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        self.usage.add(&turn_usage);
        if let Err(e) = &result {
            self.emit(Event::Error { session: self.id.clone(), error: format!("{e:#}") });
        }
        self.emit(Event::TurnComplete { session: self.id.clone(), usage: turn_usage, aborted });
        result
    }

    /// Final assistant text of the last turn.
    pub fn last_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) if !a.text().is_empty() => Some(a.text()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn tool_error(id: &str, name: &str, msg: &str) -> Message {
    Message::ToolResult {
        tool_call_id: id.into(),
        tool_name: name.into(),
        content: vec![ContentBlock::text(msg)],
        is_error: true,
    }
}

/// Front-end handle: run prompts in the background, steer, queue, interrupt.
pub struct Agent {
    pub session: Arc<tokio::sync::Mutex<Session>>,
    pub id: String,
    steer: Queue,
    follow: Queue,
    cancel: std::sync::Mutex<CancellationToken>,
    running: AtomicBool,
}

impl Agent {
    pub fn new(session: Session) -> Arc<Agent> {
        Arc::new(Agent {
            id: session.id.clone(),
            steer: session.steer.clone(),
            follow: session.follow.clone(),
            session: Arc::new(tokio::sync::Mutex::new(session)),
            cancel: std::sync::Mutex::new(CancellationToken::new()),
            running: AtomicBool::new(false),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Run a prompt to completion (awaits).
    pub async fn prompt(self: &Arc<Self>, text: String) -> anyhow::Result<()> {
        let cancel = CancellationToken::new();
        *self.cancel.lock().unwrap() = cancel.clone();
        self.running.store(true, Ordering::SeqCst);
        let r = self.session.lock().await.run(&text, cancel).await;
        self.running.store(false, Ordering::SeqCst);
        r
    }

    /// Start a prompt in the background, or steer the running one.
    pub fn submit(self: &Arc<Self>, text: String, mode: InputMode) {
        if self.is_running() {
            match mode {
                InputMode::Steer => self.steer.lock().unwrap().push_back(text),
                InputMode::FollowUp => self.follow.lock().unwrap().push_back(text),
            }
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let me = self.clone();
        tokio::spawn(async move {
            let _ = me.prompt(text).await;
        });
    }

    pub fn interrupt(&self) {
        self.cancel.lock().unwrap().cancel();
        self.steer.lock().unwrap().clear();
        self.follow.lock().unwrap().clear();
    }
}
