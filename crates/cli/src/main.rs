use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use codecraft_core::approval::{ApprovalMode, Approvals, DenyPrompter};
use codecraft_core::config::Config;
use codecraft_core::persist::{rebuild, Persist};
use codecraft_core::{agents, Session, SessionSetup};
use codecraft_protocol::{Event, Message};
use codecraft_store::Store;

mod sandbox;

#[derive(Parser)]
#[command(name = "codecraft", version, about = "A terminal coding agent")]
struct Cli {
    /// Prompt to run. With -p, runs non-interactively and exits.
    prompt: Option<String>,
    /// Print mode: run the prompt, print the answer, exit.
    #[arg(short, long)]
    print: bool,
    /// With -p: emit events as JSON lines on stdout.
    #[arg(long)]
    json: bool,
    /// Model as provider/model (e.g. anthropic/claude-sonnet-4-5, claude/sonnet, ollama/qwen3).
    #[arg(short, long)]
    model: Option<String>,
    /// Approval mode: ask, auto-edit, yolo.
    #[arg(long, value_parser = parse_mode)]
    approval: Option<ApprovalMode>,
    /// Run bash commands in a Firecracker microVM (no network).
    #[arg(long)]
    sandbox: bool,
    /// Shorthand for --approval yolo.
    #[arg(long)]
    yolo: bool,
    /// Continue the most recent session in this directory.
    #[arg(short, long)]
    r#continue: bool,
    /// Resume a session by id (prefix ok).
    #[arg(short, long)]
    resume: Option<String>,
    /// Don't save the session.
    #[arg(long)]
    no_store: bool,
    /// Working directory.
    #[arg(short = 'C', long)]
    cwd: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List, search, show or export saved sessions.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionCmd>,
    },
    /// Show configured providers and the default model.
    Models,
    /// How to log in to a provider.
    Login { provider: String },
    /// List available sub-agents.
    Agents,
    /// Manage the Firecracker sandbox (`setup`, `status`).
    Sandbox { action: String },
    /// Internal: MCP stdio server relaying codecraft tools to `claude`.
    #[command(hide = true)]
    McpServe {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    List {
        /// All directories, not just this one.
        #[arg(short, long)]
        all: bool,
    },
    Search { query: String },
    Show { id: String },
    Export { id: String },
    Rm { id: String },
}

static COLOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Style text only when color output is on.
fn paint(text: impl std::fmt::Display, color: crossterm::style::Color) -> String {
    if COLOR.load(std::sync::atomic::Ordering::Relaxed) {
        format!("{}", crossterm::style::Stylize::with(text.to_string(), color))
    } else {
        text.to_string()
    }
}

fn parse_mode(s: &str) -> Result<ApprovalMode, String> {
    match s {
        "ask" => Ok(ApprovalMode::Ask),
        "auto-edit" | "auto" => Ok(ApprovalMode::AutoEdit),
        "yolo" | "never" => Ok(ApprovalMode::Yolo),
        _ => Err("expected ask, auto-edit or yolo".into()),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("CODECRAFT_LOG"))
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run().await {
        eprintln!("{} {e:#}", paint("error:", crossterm::style::Color::Red));
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if !std::io::stderr().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        COLOR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    let cwd = match &cli.cwd {
        Some(c) => {
            let c = std::fs::canonicalize(c)?;
            std::env::set_current_dir(&c)?;
            c
        }
        None => std::env::current_dir()?,
    };
    match &cli.cmd {
        Some(Cmd::McpServe { socket }) => return codecraft_llm::claude_bridge::mcp_serve(socket.clone()).await,
        Some(Cmd::Sessions { action }) => return sessions_cmd(action.as_ref(), &cwd),
        Some(Cmd::Models) => {
            let cfg = Config::load(&cwd)?;
            println!("default model: {}", cfg.default_model());
            for (id, p) in &cfg.providers {
                let ready = match &p.env_key {
                    Some(k) => std::env::var(k).is_ok() || p.api_key.is_some(),
                    None => true,
                };
                println!(
                    "  {:<12} {:<12} {}",
                    id,
                    format!("{:?}", p.kind).to_lowercase(),
                    if ready { paint("ready", crossterm::style::Color::Green) } else { paint(format!("needs ${}", p.env_key.clone().unwrap_or_default()), crossterm::style::Color::Yellow) }
                );
            }
            return Ok(());
        }
        Some(Cmd::Login { provider }) => {
            match provider.as_str() {
                "claude" => println!(
                    "codecraft uses your own Claude Code login. Run:\n\n  claude auth login\n\nthen use `-m claude/sonnet` (or claude/opus, claude/default)."
                ),
                "anthropic" => println!("Set ANTHROPIC_API_KEY (console.anthropic.com), then use `-m anthropic/<model>`."),
                "openai" => println!("Set OPENAI_API_KEY, then use `-m openai/<model>`."),
                p => println!("Set the API key for `{p}` in its env var (see `codecraft models`)."),
            }
            return Ok(());
        }
        Some(Cmd::Sandbox { action }) => return sandbox::cmd(action).await,
        Some(Cmd::Agents) => {
            for a in agents::load(&cwd) {
                println!("{:<10} {}", a.name.clone(), a.description);
            }
            return Ok(());
        }
        None => {}
    }

    let mut cfg = Config::load(&cwd)?;
    if cli.sandbox {
        cfg.sandbox = "firecracker".into();
    }
    if cli.yolo {
        cfg.approval = ApprovalMode::Yolo;
    } else if let Some(m) = cli.approval {
        cfg.approval = m;
    }
    let store: Option<Arc<Store>> = if cli.no_store { None } else { Some(Arc::new(Store::open_default()?)) };

    // Resume?
    let mut resumed: Option<(String, Vec<Message>, String)> = None;
    if let Some(s) = &store {
        let row = if let Some(id) = &cli.resume {
            Some(s.get(id)?.with_context(|| format!("no session `{id}`"))?)
        } else if cli.r#continue {
            Some(s.latest(&cwd.display().to_string())?.context("no previous session in this directory")?)
        } else {
            None
        };
        if let Some(row) = row {
            let msgs = rebuild(&s.entries(&row.id)?);
            resumed = Some((row.id, msgs, row.model));
        }
    }
    let model = cli
        .model
        .clone()
        .or_else(|| resumed.as_ref().map(|r| r.2.clone()))
        .unwrap_or_else(|| cfg.default_model());

    let mut prompt = cli.prompt.clone();
    if cli.print && prompt.is_none() && !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        prompt = Some(s);
    }
    let cfg = Arc::new(cfg);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let persist = store.clone().map(|s| s as Arc<dyn Persist>);

    if cli.print {
        let Some(prompt) = prompt else { bail!("-p needs a prompt (argument or stdin)") };
        // Non-interactive: anything needing approval is denied unless --yolo / auto-edit.
        let approvals = Arc::new(Approvals::new(cfg.approval, Box::new(DenyPrompter)));
        let setup = SessionSetup {
            config: cfg.clone(),
            cwd: cwd.clone(),
            model,
            approvals,
            events: tx,
            persist,
            agents: agents::load(&cwd),
            agent: None,
            depth: 0,
            parent_id: None,
            id: resumed.as_ref().map(|r| r.0.clone()),
            messages: resumed.map(|r| r.1).unwrap_or_default(),
        };
        let mut session = Session::new(setup)?;
        let printer = tokio::spawn(print_events(rx, session.id.clone(), cli.json));
        let cancel = tokio_util::sync::CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            c2.cancel();
        });
        let r = session.run(&prompt, cancel).await;
        drop(session);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), printer).await;
        return r;
    }

    codecraft_tui::run(codecraft_tui::TuiArgs {
        config: cfg,
        cwd,
        model,
        store,
        resumed: resumed.map(|r| (r.0, r.1)),
        initial_prompt: prompt,
        events_tx: tx,
        events_rx: rx,
    })
    .await
}

async fn print_events(mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>, main: String, json: bool) {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut at_line_start = true;
    while let Some(e) = rx.recv().await {
        let is_main = e.session() == main;
        let done = is_main && matches!(e, Event::TurnComplete { .. });
        if json {
            let _ = writeln!(out, "{}", serde_json::to_string(&e).unwrap_or_default());
            if done {
                break;
            }
            continue;
        }
        match e {
            Event::TextDelta { text, .. } if is_main => {
                let _ = write!(out, "{text}");
                let _ = out.flush();
                at_line_start = text.ends_with('\n');
            }
            Event::ToolStart { name, arguments, .. } => {
                if !at_line_start {
                    let _ = writeln!(out);
                    at_line_start = true;
                }
                let indent = if is_main { "" } else { "    " };
                let _ = writeln!(err, "{indent}{} {}", paint(format!("● {name}"), crossterm::style::Color::Cyan), paint(codecraft_tui::summarize_args(&name, &arguments), crossterm::style::Color::DarkGrey));
            }
            Event::ToolEnd { is_error: true, output, .. } => {
                let indent = if is_main { "  " } else { "      " };
                let first = output.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                let _ = writeln!(err, "{indent}{}", paint(format!("✗ {first}"), crossterm::style::Color::Red));
            }
            Event::SubagentStarted { agent, description, .. } => {
                let _ = writeln!(err, "{}", paint(format!("  ↳ {agent}: {description}"), crossterm::style::Color::Magenta));
            }
            Event::Retrying { attempt, error, .. } => {
                let _ = writeln!(err, "{}", paint(format!("retry {attempt}: {error}"), crossterm::style::Color::Yellow));
            }
            Event::Compacted { tokens_before, .. } => {
                let _ = writeln!(err, "{}", paint(format!("[compacted context, was ~{tokens_before} tokens]"), crossterm::style::Color::DarkGrey));
            }
            Event::Error { error, session } if session == main => {
                let _ = writeln!(err, "{} {error}", paint("error:", crossterm::style::Color::Red));
            }
            Event::TurnComplete { usage, session, .. } if session == main => {
                if !at_line_start {
                    let _ = writeln!(out);
                }
                let _ = writeln!(
                    err,
                    "{}",
                    paint(format!("[{} in / {} out / {} cached tokens]", usage.input, usage.output, usage.cache_read), crossterm::style::Color::DarkGrey)
                );
            }
            _ => {}
        }
        if done {
            break;
        }
    }
}

fn sessions_cmd(action: Option<&SessionCmd>, cwd: &std::path::Path) -> anyhow::Result<()> {
    let store = Store::open_default()?;
    let show_row = |r: &codecraft_store::SessionRow| {
        println!(
            "{}  {}  {:<28} {}",
            r.id[..13.min(r.id.len())].to_string(),
            r.updated_at.get(..16).unwrap_or(&r.updated_at).replace('T', " "),
            r.model,
            r.title.clone().unwrap_or_default().replace('\n', " ").chars().take(60).collect::<String>()
        )
    };
    match action {
        None | Some(SessionCmd::List { all: false }) => {
            for r in store.list(Some(&cwd.display().to_string()), 30)? {
                show_row(&r);
            }
        }
        Some(SessionCmd::List { all: true }) => {
            for r in store.list(None, 50)? {
                show_row(&r);
            }
        }
        Some(SessionCmd::Search { query }) => {
            for (r, snip) in store.search(query, 20)? {
                show_row(&r);
                println!("    {}", paint(snip.replace('\n', " "), crossterm::style::Color::DarkGrey));
            }
        }
        Some(SessionCmd::Show { id }) => {
            let row = store.get(id)?.context("no such session")?;
            show_row(&row);
            for m in rebuild(&store.entries(&row.id)?) {
                match m {
                    Message::User { .. } => {
                        println!("\n{} {}", paint("you:", crossterm::style::Color::Green), message_text(&m))
                    }
                    Message::Assistant(a) => {
                        let t = a.text();
                        if !t.is_empty() {
                            println!("\n{} {t}", paint("codecraft:", crossterm::style::Color::Cyan));
                        }
                        for (_, n, args) in a.tool_calls() {
                            println!("  {} {}", paint(format!("● {n}"), crossterm::style::Color::Cyan), paint(codecraft_tui::summarize_args(n, args), crossterm::style::Color::DarkGrey));
                        }
                    }
                    Message::ToolResult { .. } => {}
                }
            }
            for c in store.children(&row.id)? {
                println!("\n  sub-agent {} ({})", c.id.clone(), c.agent.unwrap_or_default());
            }
        }
        Some(SessionCmd::Export { id }) => print!("{}", store.export_jsonl(id)?),
        Some(SessionCmd::Rm { id }) => {
            let row = store.get(id)?.context("no such session")?;
            store.delete(&row.id)?;
            println!("deleted {}", row.id);
        }
    }
    Ok(())
}

fn message_text(m: &Message) -> String {
    match m {
        Message::User { content } => content
            .iter()
            .filter_map(|b| match b {
                codecraft_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
