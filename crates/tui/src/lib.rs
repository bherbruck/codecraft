//! Interactive terminal UI. Inline viewport: finished output is inserted into
//! the terminal's own scrollback; only the live area and composer redraw.

mod composer;
mod render;

use std::collections::HashMap;
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use codecraft_core::approval::{ApprovalMode, Approvals, Prompter};
use codecraft_core::config::Config;
use codecraft_core::persist::{rebuild, Persist};
use codecraft_core::{agents, Agent, Session, SessionSetup};
use codecraft_protocol::*;
use codecraft_store::Store;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

pub use render::summarize_args;
use render::{dim, styled_block, Md};

pub struct TuiArgs {
    pub config: Arc<Config>,
    pub cwd: PathBuf,
    pub model: String,
    pub store: Option<Arc<Store>>,
    pub resumed: Option<(String, Vec<Message>)>,
    pub initial_prompt: Option<String>,
    pub events_tx: UnboundedSender<Event>,
    pub events_rx: UnboundedReceiver<Event>,
}

type ApprovalMsg = (ApprovalRequest, oneshot::Sender<ApprovalDecision>);

struct TuiPrompter(UnboundedSender<ApprovalMsg>);

#[async_trait]
impl Prompter for TuiPrompter {
    async fn ask(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        if self.0.send((req, tx)).is_err() {
            return ApprovalDecision::Deny;
        }
        rx.await.unwrap_or(ApprovalDecision::Deny)
    }
}

const VIEWPORT: u16 = 9;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct App {
    args_config: Arc<Config>,
    cwd: PathBuf,
    store: Option<Arc<Store>>,
    events_tx: UnboundedSender<Event>,
    approvals: Arc<Approvals>,
    agent: Arc<Agent>,
    model: String,
    composer: composer::Composer,
    /// Partial (unterminated) line of streamed main-session text.
    stream: String,
    thinking: String,
    md: Md,
    running_since: Option<Instant>,
    tick: usize,
    usage: Usage,
    last_context: u64,
    approval: Option<ApprovalMsg>,
    tools: HashMap<String, (String, Value)>,
    subagents: HashMap<String, String>,
    quit: bool,
    pending_lines: Vec<Line<'static>>,
    width: u16,
}

use serde_json::Value;

impl App {
    fn session_setup(&self, model: &str, id: Option<String>, messages: Vec<Message>) -> SessionSetup {
        SessionSetup {
            config: self.args_config.clone(),
            cwd: self.cwd.clone(),
            model: model.to_string(),
            approvals: self.approvals.clone(),
            events: self.events_tx.clone(),
            persist: self.store.clone().map(|s| s as Arc<dyn Persist>),
            agents: agents::load(&self.cwd),
            agent: None,
            depth: 0,
            parent_id: None,
            id,
            messages,
        }
    }

    fn push(&mut self, lines: Vec<Line<'static>>) {
        self.pending_lines.extend(lines);
    }

    fn note(&mut self, text: &str, style: Style) {
        let w = self.width;
        self.push(styled_block("", text, style, w));
    }

    fn flush_stream(&mut self) {
        if !self.stream.is_empty() {
            let s = std::mem::take(&mut self.stream);
            let w = self.width;
            let l = self.md.line(&s, w);
            self.push(l);
        }
    }

    fn flush_thinking(&mut self) {
        if !self.thinking.trim().is_empty() {
            let t = std::mem::take(&mut self.thinking);
            let w = self.width;
            self.push(styled_block("  ", t.trim(), dim().add_modifier(Modifier::ITALIC), w));
        }
        self.thinking.clear();
    }

    fn on_event(&mut self, e: Event) {
        let main = e.session() == self.agent.id;
        let sub_prefix = |app: &App, s: &str| app.subagents.get(s).cloned();
        match e {
            Event::TurnStarted { .. } if main => {
                self.md = Md::default();
            }
            Event::TextDelta { text, .. } if main => {
                self.flush_thinking();
                self.stream.push_str(&text);
                while let Some(pos) = self.stream.find('\n') {
                    let line: String = self.stream.drain(..=pos).collect();
                    let w = self.width;
                    let l = self.md.line(line.trim_end_matches('\n'), w);
                    self.push(l);
                }
            }
            Event::ThinkingDelta { text, .. } if main => {
                self.thinking.push_str(&text);
            }
            Event::AssistantMessage { message, .. } if main => {
                self.flush_thinking();
                self.flush_stream();
                if message.usage.context_tokens() > 0 {
                    self.last_context = message.usage.context_tokens();
                }
                if let Some(err) = message.error {
                    self.note(&err, Style::default().fg(Color::Red));
                }
            }
            Event::ToolStart { session, id, name, arguments } => {
                self.flush_thinking();
                self.flush_stream();
                let summary = summarize_args(&name, &arguments);
                let w = self.width;
                let lines = match sub_prefix(self, &session) {
                    Some(_) => styled_block(&format!("    ⎿ {name} "), &summary, dim(), w),
                    None if name == "task" => vec![],
                    None => {
                        let mut l = vec![Line::from(vec![
                            Span::styled("● ", Style::default().fg(Color::Cyan)),
                            Span::styled(name.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                            Span::raw(" "),
                            Span::styled(summary, dim()),
                        ])];
                        if name == "edit" || name == "write" {
                            l.truncate(1);
                        }
                        l
                    }
                };
                self.push(lines);
                self.tools.insert(id, (name, arguments));
            }
            Event::ToolEnd { session, id, name, output, is_error } => {
                self.tools.remove(&id);
                let sub = sub_prefix(self, &session).is_some();
                let w = self.width;
                if is_error {
                    let first: String = output.lines().next().unwrap_or("").chars().take(200).collect();
                    let p = if sub { "      ✗ " } else { "  ✗ " };
                    self.push(styled_block(p, &first, Style::default().fg(Color::Red), w));
                } else if !sub && name == "edit" {
                    // Show the diff.
                    let mut lines = Vec::new();
                    for l in output.lines().skip(3).take(40) {
                        let style = if l.starts_with('+') {
                            Style::default().fg(Color::Green)
                        } else if l.starts_with('-') {
                            Style::default().fg(Color::Red)
                        } else {
                            dim()
                        };
                        lines.extend(styled_block("    ", l, style, w));
                    }
                    self.push(lines);
                } else if !sub && name == "bash" {
                    let all: Vec<&str> = output.lines().collect();
                    let n = all.len();
                    let mut lines = Vec::new();
                    for l in all.iter().take(4) {
                        let l: String = l.chars().take(w as usize - 6).collect();
                        lines.push(Line::from(Span::styled(format!("    {l}"), dim())));
                    }
                    if n > 4 {
                        lines.push(Line::from(Span::styled(format!("    … +{} lines", n - 4), dim())));
                    }
                    self.push(lines);
                } else if !sub && name != "task" {
                    let n = output.lines().count();
                    self.push(vec![Line::from(Span::styled(format!("  ⎿ {n} line{}", if n == 1 { "" } else { "s" }), dim()))]);
                }
            }
            Event::SubagentStarted { session, agent, description, .. } => {
                self.flush_stream();
                self.subagents.insert(session, agent.clone());
                self.push(vec![Line::from(vec![
                    Span::styled("● ", Style::default().fg(Color::Magenta)),
                    Span::styled(format!("task({agent})"), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(description, dim()),
                ])]);
            }
            Event::SubagentFinished { session, .. } => {
                if let Some(a) = self.subagents.remove(&session) {
                    self.push(vec![Line::from(Span::styled(format!("    ⎿ {a} done"), Style::default().fg(Color::Magenta)))]);
                }
            }
            Event::Compacted { tokens_before, .. } if main => {
                self.note(&format!("[context compacted; was ~{tokens_before} tokens]"), dim());
            }
            Event::Retrying { attempt, error, .. } if main => {
                let e: String = error.chars().take(200).collect();
                self.note(&format!("retry {attempt}: {e}"), Style::default().fg(Color::Yellow));
            }
            Event::Info { text, .. } if main => self.note(&text, dim()),
            Event::Error { error, .. } if main => {
                self.flush_stream();
                self.note(&format!("error: {error}"), Style::default().fg(Color::Red));
            }
            Event::TurnComplete { usage, aborted, .. } if main => {
                self.flush_thinking();
                self.flush_stream();
                self.usage.add(&usage);
                self.running_since = None;
                if aborted {
                    self.note("[interrupted]", Style::default().fg(Color::Yellow));
                }
                self.push(vec![Line::from("")]);
            }
            _ => {}
        }
    }

    fn submit(&mut self, text: String, mode: InputMode) {
        let t = text.trim().to_string();
        if t.is_empty() {
            return;
        }
        if t.starts_with('/') && !self.agent.is_running() {
            self.command(&t);
            return;
        }
        let w = self.width;
        let style = Style::default().fg(Color::Green);
        let prefix = if self.agent.is_running() {
            if mode == InputMode::Steer { "↪ " } else { "⏳ " }
        } else {
            "› "
        };
        self.push(styled_block(prefix, &t, style.add_modifier(Modifier::BOLD), w));
        if !self.agent.is_running() {
            self.running_since = Some(Instant::now());
        }
        self.agent.submit(t, mode);
    }

    fn new_session(&mut self, model: &str, id: Option<String>, messages: Vec<Message>) {
        match Session::new(self.session_setup(model, id, messages)) {
            Ok(s) => {
                self.agent = Agent::new(s);
                self.model = model.to_string();
            }
            Err(e) => self.note(&format!("error: {e:#}"), Style::default().fg(Color::Red)),
        }
    }

    fn command(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match name {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/help" | "/?" => self.note(
                "/model [provider/model]  switch or show model\n/clear  start a new session\n/compact  summarize context now\n\
                 /resume [id]  list or resume sessions\n/approval [ask|auto-edit|yolo]\n/agents  list sub-agents\n\
                 /cost  usage so far\n/quit\n\nEnter send · Ctrl+J newline · while running: Enter steers, Alt+Enter queues · Esc interrupts",
                dim(),
            ),
            "/model" if arg.is_empty() => {
                let m = self.model.clone();
                self.note(&format!("model: {m}"), dim())
            }
            "/model" => {
                let res = self.agent.session.try_lock().map(|mut s| s.set_model(arg));
                match res {
                    Ok(Ok(())) => {
                        self.model = arg.to_string();
                        self.note(&format!("model → {arg}"), dim());
                    }
                    Ok(Err(e)) => self.note(&format!("error: {e:#}"), Style::default().fg(Color::Red)),
                    Err(_) => self.note("busy", dim()),
                }
            }
            "/clear" | "/new" => {
                let m = self.model.clone();
                self.new_session(&m, None, vec![]);
                self.usage = Usage::default();
                self.last_context = 0;
                self.note("[new session]", dim());
            }
            "/compact" => {
                let agent = self.agent.clone();
                let tx = self.events_tx.clone();
                let id = agent.id.clone();
                self.running_since = Some(Instant::now());
                tokio::spawn(async move {
                    let mut s = agent.session.lock().await;
                    let r = s.compact(tokio_util::sync::CancellationToken::new()).await;
                    let text = match r {
                        Ok(true) => None,
                        Ok(false) => Some("nothing to compact".to_string()),
                        Err(e) => Some(format!("compaction failed: {e:#}")),
                    };
                    if let Some(text) = text {
                        let _ = tx.send(Event::Info { session: id.clone(), text });
                    }
                    let _ = tx.send(Event::TurnComplete { session: id, usage: Usage::default(), aborted: false });
                });
            }
            "/approval" => {
                let mode = match arg {
                    "ask" => Some(ApprovalMode::Ask),
                    "auto-edit" | "auto" => Some(ApprovalMode::AutoEdit),
                    "yolo" => Some(ApprovalMode::Yolo),
                    _ => None,
                };
                match mode {
                    Some(m) => {
                        *self.approvals.mode.lock().unwrap() = m;
                        self.note(&format!("approval → {arg}"), dim());
                    }
                    None => {
                        let m = *self.approvals.mode.lock().unwrap();
                        self.note(&format!("approval: {m:?} (ask | auto-edit | yolo)"), dim())
                    }
                }
            }
            "/agents" => {
                let text: Vec<String> = agents::load(&self.cwd).iter().map(|a| format!("{}: {}", a.name, a.description)).collect();
                self.note(&text.join("\n"), dim());
            }
            "/cost" => {
                let u = self.usage;
                self.note(
                    &format!("tokens: {} in, {} out, {} cache read, {} cache write · context ~{}", u.input, u.output, u.cache_read, u.cache_write, self.last_context),
                    dim(),
                );
            }
            "/resume" | "/sessions" => {
                let Some(store) = self.store.clone() else {
                    self.note("sessions are not being saved (--no-store)", dim());
                    return;
                };
                if arg.is_empty() {
                    let rows = store.list(Some(&self.cwd.display().to_string()), 15).unwrap_or_default();
                    let text: Vec<String> = rows
                        .iter()
                        .map(|r| {
                            format!(
                                "{}  {}  {}",
                                &r.id[..13.min(r.id.len())],
                                r.updated_at.get(..16).unwrap_or("").replace('T', " "),
                                r.title.clone().unwrap_or_default().replace('\n', " ").chars().take(60).collect::<String>()
                            )
                        })
                        .collect();
                    self.note(&format!("{}\n\n/resume <id> to continue one", text.join("\n")), dim());
                } else {
                    match store.get(arg) {
                        Ok(Some(row)) => {
                            let msgs = rebuild(&store.entries(&row.id).unwrap_or_default());
                            let n = msgs.len();
                            self.new_session(&row.model.clone(), Some(row.id.clone()), msgs);
                            self.note(&format!("[resumed {} · {n} messages]", row.id), dim());
                        }
                        Ok(None) => self.note("no such session", dim()),
                        Err(e) => self.note(&format!("error: {e:#}"), Style::default().fg(Color::Red)),
                    }
                }
            }
            _ => self.note(&format!("unknown command {name} (try /help)"), dim()),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if let Some((_, _)) = &self.approval {
            let decision = match k.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalDecision::Once),
                KeyCode::Char('a') => Some(ApprovalDecision::Session),
                KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
                _ => None,
            };
            if let Some(d) = decision {
                let (req, tx) = self.approval.take().unwrap();
                let w = self.width;
                let (label, style) = match d {
                    ApprovalDecision::Once => ("allowed", Style::default().fg(Color::Green)),
                    ApprovalDecision::Session => ("allowed for session", Style::default().fg(Color::Green)),
                    ApprovalDecision::Deny => ("denied", Style::default().fg(Color::Red)),
                };
                self.push(styled_block("  ", &format!("{label}: {}", req.summary.lines().next().unwrap_or("")), style, w));
                let _ = tx.send(d);
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if self.agent.is_running() {
                    self.agent.interrupt();
                } else if !self.composer.is_empty() {
                    self.composer.clear();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.composer.is_empty() => self.quit = true,
            KeyCode::Esc if self.agent.is_running() => self.agent.interrupt(),
            KeyCode::Enter if alt => {
                let t = self.composer.take();
                self.submit(t, InputMode::FollowUp);
            }
            KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => self.composer.insert('\n'),
            KeyCode::Char('j') if ctrl => self.composer.insert('\n'),
            KeyCode::Enter => {
                let t = self.composer.take();
                self.submit(t, InputMode::Steer);
            }
            _ => self.composer.key(k),
        }
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let area = f.area();
        let composer_h = (self.composer.line_count().min(4) as u16) + 2;
        let [live, status, input] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(composer_h)]).areas(area);

        // Live area: approval prompt, or the partial streamed line.
        if let Some((req, _)) = &self.approval {
            let lines = vec![
                Line::from(vec![
                    Span::styled("Allow ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(req.tool.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::raw(": "),
                    Span::raw(req.summary.clone()),
                ]),
                Line::from(vec![
                    Span::styled("[y]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::raw(" yes   "),
                    Span::styled("[a]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                    Span::raw(format!(" always ({}) this session   ", req.cache_key)),
                    Span::styled("[n]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    Span::raw(" no"),
                ]),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: false }).render(live, f.buffer_mut());
        } else {
            let mut lines = Vec::new();
            if !self.thinking.is_empty() {
                let t: String = self.thinking.lines().last().unwrap_or("").chars().take(live.width as usize).collect();
                lines.push(Line::from(Span::styled(t, dim().add_modifier(Modifier::ITALIC))));
            }
            if !self.stream.is_empty() {
                lines.push(Line::from(self.stream.clone()));
            }
            let skip = lines.len().saturating_sub(live.height as usize);
            Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>())
                .wrap(Wrap { trim: false })
                .render(live, f.buffer_mut());
        }

        // Status line.
        let status_line = match self.running_since {
            Some(t) => {
                let running_tools: Vec<String> = self.tools.values().map(|(n, _)| n.clone()).collect();
                let what = if !self.subagents.is_empty() {
                    format!("{} sub-agent(s)", self.subagents.len())
                } else if !running_tools.is_empty() {
                    running_tools.join(", ")
                } else if !self.thinking.is_empty() && self.stream.is_empty() {
                    "thinking".into()
                } else {
                    "working".into()
                };
                Line::from(vec![
                    Span::styled(format!("{} ", SPINNER[self.tick % SPINNER.len()]), Style::default().fg(Color::Cyan)),
                    Span::styled(format!("{what} {}s", t.elapsed().as_secs()), Style::default().fg(Color::Cyan)),
                    Span::styled("  · esc to interrupt · enter to steer", dim()),
                ])
            }
            None => {
                let mode = format!("{:?}", *self.approvals.mode.lock().unwrap()).to_lowercase();
                let ctx = if self.last_context > 0 {
                    let w = self.args_config.context_window(&self.model);
                    format!(" · {}% context", self.last_context * 100 / w.max(1))
                } else {
                    String::new()
                };
                Line::from(Span::styled(format!("{} · {mode}{ctx} · /help", self.model), dim()))
            }
        };
        Paragraph::new(status_line).render(status, f.buffer_mut());

        // Composer.
        let block = Block::default().borders(Borders::TOP | Borders::BOTTOM).border_style(dim());
        let inner = block.inner(input);
        block.render(input, f.buffer_mut());
        let (lines, cursor) = self.composer.view(inner.width.saturating_sub(2), inner.height);
        let mut display: Vec<Line> = Vec::new();
        for (i, l) in lines.into_iter().enumerate() {
            let p = if i == 0 { "› " } else { "  " };
            display.push(Line::from(vec![Span::styled(p, Style::default().fg(Color::Green)), Span::raw(l)]));
        }
        if self.composer.is_empty() {
            display = vec![Line::from(vec![
                Span::styled("› ", Style::default().fg(Color::Green)),
                Span::styled(if self.agent.is_running() { "steer the agent…" } else { "ask codecraft… (/help)" }, dim()),
            ])];
        }
        Paragraph::new(display).render(inner, f.buffer_mut());
        if self.approval.is_none() {
            f.set_cursor_position((inner.x + 2 + cursor.0, inner.y + cursor.1));
        }
    }
}

fn flush_lines(terminal: &mut Terminal<CrosstermBackend<Stdout>>, lines: Vec<Line<'static>>) -> anyhow::Result<()> {
    let size = terminal.size()?;
    let chunk = (size.height.saturating_sub(VIEWPORT)).max(1) as usize;
    for part in lines.chunks(chunk) {
        let part = part.to_vec();
        terminal.insert_before(part.len() as u16, |buf| {
            Paragraph::new(part).render(buf.area, buf);
        })?;
    }
    Ok(())
}

pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    let (appr_tx, mut appr_rx) = tokio::sync::mpsc::unbounded_channel::<ApprovalMsg>();
    let approvals = Arc::new(Approvals::new(args.config.approval, Box::new(TuiPrompter(appr_tx))));
    let mut events_rx = args.events_rx;

    let resumed_n = args.resumed.as_ref().map(|r| r.1.len());
    let (rid, rmsgs) = args.resumed.map(|(i, m)| (Some(i), m)).unwrap_or((None, vec![]));
    let setup = SessionSetup {
        config: args.config.clone(),
        cwd: args.cwd.clone(),
        model: args.model.clone(),
        approvals: approvals.clone(),
        events: args.events_tx.clone(),
        persist: args.store.clone().map(|s| s as Arc<dyn Persist>),
        agents: agents::load(&args.cwd),
        agent: None,
        depth: 0,
        parent_id: None,
        id: rid,
        messages: rmsgs,
    };
    let session = Session::new(setup)?;
    let agent = Agent::new(session);

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(VIEWPORT) })?;

    let width = terminal.size()?.width;
    let mut app = App {
        args_config: args.config,
        cwd: args.cwd.clone(),
        store: args.store,
        events_tx: args.events_tx,
        approvals,
        agent,
        model: args.model,
        composer: composer::Composer::default(),
        stream: String::new(),
        thinking: String::new(),
        md: Md::default(),
        running_since: None,
        tick: 0,
        usage: Usage::default(),
        last_context: 0,
        approval: None,
        tools: HashMap::new(),
        subagents: HashMap::new(),
        quit: false,
        pending_lines: vec![],
        width,
    };
    app.push(vec![
        Line::from(vec![
            Span::styled("codecraft", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" v{} · {} · {}", env!("CARGO_PKG_VERSION"), app.model, args.cwd.display()), dim()),
        ]),
        Line::from(""),
    ]);
    if let Some(n) = resumed_n {
        app.note(&format!("[resumed {} · {n} messages]", app.agent.id), dim());
    }
    if let Some(p) = args.initial_prompt {
        app.submit(p, InputMode::Steer);
    }

    let mut term_events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let result: anyhow::Result<()> = async {
        loop {
            let lines = std::mem::take(&mut app.pending_lines);
            if !lines.is_empty() {
                flush_lines(&mut terminal, lines)?;
            }
            terminal.draw(|f| app.draw(f))?;
            if app.quit {
                break;
            }
            tokio::select! {
                Some(ev) = term_events.next() => {
                    match ev? {
                        TermEvent::Key(k) => app.on_key(k),
                        TermEvent::Paste(s) => app.composer.insert_str(&s.replace("\r\n", "\n").replace('\r', "\n")),
                        TermEvent::Resize(w, _) => {
                            app.width = w;
                            terminal.autoresize()?;
                        }
                        _ => {}
                    }
                }
                Some(e) = events_rx.recv() => {
                    app.on_event(e);
                    // Drain bursts of deltas before redrawing.
                    while let Ok(e) = events_rx.try_recv() {
                        app.on_event(e);
                    }
                }
                Some(a) = appr_rx.recv() => {
                    app.flush_stream();
                    app.approval = Some(a);
                }
                _ = ticker.tick() => {
                    app.tick += 1;
                }
            }
        }
        Ok(())
    }
    .await;

    app.agent.interrupt();
    terminal.clear()?;
    execute!(std::io::stdout(), DisableBracketedPaste)?;
    crossterm::terminal::disable_raw_mode()?;
    println!();
    if app.store.is_some() {
        println!("session {} · resume with `codecraft -r {}`", app.agent.id, &app.agent.id[..13.min(app.agent.id.len())]);
    }
    result
}
