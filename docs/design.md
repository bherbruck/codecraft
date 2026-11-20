# codecraft — design v0

A Rust terminal coding agent. Small core (pi), multi-provider + Claude via the
official `claude` CLI (opencode), Rust engine structure + safety (codex).

Reference sources: `refs/pi`, `refs/opencode`, `refs/codex` (git-ignored, shallow clones 2026-11-19).

## 1. Crates (Cargo workspace, 7 crates)

| Crate | Job | Borrowed from |
|---|---|---|
| `codecraft-protocol` | Pure types: `Op` (in), `Event` (out), `Message`, `ContentBlock`, `ToolCall`, `Usage`, `StopReason`. No logic. | codex `protocol` |
| `codecraft-llm` | `Provider` trait → stream of neutral `StreamEvent`s. Adapters: Anthropic Messages, OpenAI Chat Completions (+ any compatible), OpenAI Responses, Claude CLI bridge. Model catalog (TOML, data not code). | pi `packages/ai` |
| `codecraft-core` | Session task, turn loop, tool router, compaction, AGENTS.md loader, approvals, JSONL sessions. | codex `core` + pi `agent` |
| `codecraft-tools` | Built-in tools + truncation utils + edit/patch matcher. | pi tools, codex `apply-patch` |
| `codecraft-store` | SQLite (rusqlite, bundled): sessions, entries, approvals, search (FTS5). JSONL export/import. | opencode (SQLite), codex `state` |
| `codecraft-tui` | ratatui inline viewport; finished history goes to native scrollback. | codex `tui` |
| `codecraft-cli` | `clap` entry: interactive, `-p` print mode, `--json` events, `resume`, `login`, `mcp-serve` (internal). Builds `bin/codecraft`. | — |

Key deps: tokio, tokio-util (`CancellationToken`), reqwest + eventsource-stream, serde/serde_json, toml, clap 4, ratatui 0.30 + crossterm 0.29, pulldown-cmark, similar, ignore + globset (gitignore-aware walking), grep crates (ripgrep internals), rmcp (MCP), thiserror/anyhow, tracing.

## 2. Engine model

- Front-ends send `Op`s (`UserInput{mode: Steer|FollowUp}`, `Interrupt`, `Approve{id, decision}`, `Compact`, `Shutdown`) over an async channel; core emits `Event`s. TUI, print mode, and tests share one engine. (codex)
- Turn loop: stream → collect tool calls → run tools → if tool calls or steering pending, sample again; else check follow-up queue; else stop. (pi + codex)
- Tools start as soon as each tool call finishes streaming (`FuturesOrdered`, results returned in call order). Parallel-safe tools take a read lock, others a write lock. Per-file mutation lock on realpath. (codex + pi)
- `stop_reason == length` → every tool call in that message fails with "output truncated, retry", never executed. (pi)
- `CancellationToken` everywhere; interrupted calls get synthetic "aborted" results so call/result pairs stay valid. (codex)
- Retries: 3, jittered backoff; context overflow → compact + retry once.
- Auto-compaction at min(config, 90% of window): structured markdown summary (Goal / Constraints / Progress / Decisions / Next steps / Critical context), updated incrementally; transcript never deleted. (pi + codex)

## 3. Providers

| Provider | Auth | Notes |
|---|---|---|
| Anthropic API | `ANTHROPIC_API_KEY` (x-api-key) | Prompt caching breakpoints on system, last tool, last user msg. Thinking with signature round-trip. |
| OpenAI | `OPENAI_API_KEY` | Responses API (reasoning) + Chat Completions. |
| OpenAI-compatible | per-provider `env_key`, `base_url` | Ollama, OpenRouter, Groq, LM Studio, etc. |
| **Claude (subscription)** | user's own `claude auth login` | See §4. |

Neutral thinking level (`off|low|medium|high|max`) mapped per model in the catalog.

## 4. Claude subscription — "claude bridge" provider

How opencode does it now (refs/opencode `packages/core/src/plugin/provider/claude-agent/`): it runs the
user's installed `claude` binary through Anthropic's Agent SDK, and exposes its own tools to it over MCP.
opencode *removed* direct Pro/Max OAuth in 1.3.0 because Anthropic prohibits it. pi still ships a spoofing
version ("stealth mode"). **codecraft does not spoof.** It does what opencode does:

1. Spawn `claude -p --input-format stream-json --output-format stream-json --verbose` with:
   built-in tools disabled, `--strict-mcp-config`, an MCP config pointing at `codecraft mcp-serve`
   (codecraft's own tools), permission mode that defers to codecraft, `--append-system-prompt` for
   codecraft context, CLAUDE.md auto-loading disabled (codecraft loads AGENTS.md/CLAUDE.md itself).
2. `codecraft mcp-serve` talks back to the running session over a local socket, so tool calls run
   through codecraft's approvals and show in codecraft's TUI.
3. Parse stream-json events → neutral `StreamEvent`s. Cost shown as $0 (subscription).
4. One long-lived `claude` process per session; kill after 5 min idle; resume via `--resume <id>`.
   Switching models mid-session sends unseen history as one checkpoint message. (opencode)
5. Login = tell the user to run `claude auth login`. codecraft never reads Claude's credential files.

Requires `claude` ≥ the version that supports these flags (local: 2.1.329). Exact flags verified against
`claude --help` during implementation.

## 5. Tools (v0)

| Tool | Params | Behavior |
|---|---|---|
| `read` | `path, offset?, limit?` | 2000 lines / 50 KB head cap, "use offset=N" hint. Images as attachments. |
| `write` | `path, content` | Creates parent dirs. |
| `edit` | `path, edits:[{old_text,new_text}]` | All matched against original; unique; non-overlapping; exact then fuzzy (NFKC, trailing ws, smart quotes, dashes); keep BOM + CRLF. (pi) |
| `apply_patch` | `patch` (string) | codex patch format, whitespace-tolerant `seek_sequence`. Offered to OpenAI models; `edit` to others (configurable). |
| `bash` | `command, timeout?, workdir?` | Head+tail truncation with warning header; full output to temp file; kill process tree on timeout/abort. |
| `grep` | `pattern, path?, glob?, ignore_case?, literal?, context?, limit?` | ripgrep crates, gitignore-aware. |
| `glob` | `pattern, path?, limit?` | gitignore-aware. |
| `todo` | `items` | Plan list shown in TUI. |

Later: PTY `exec_command`/`write_stdin` (codex), `webfetch`, MCP client tools, sub-agents.

## 6. Approvals & safety

- Modes: `ask` (default: ask before write/edit/bash outside known-safe list), `auto-edit` (edits free, bash asks), `yolo`.
- Decisions: once / for session / deny. Known-safe read-only commands list from codex `shell-command`.
- Sandbox: **Firecracker microVM** (spike, M6). This machine has `/dev/kvm` and the user is in the `kvm` group.
  - Tools (`bash`, writes) run inside a microVM via a small guest agent over vsock.
  - Workspace sharing is the hard part: Firecracker has no virtiofs. Options: (a) workspace on an ext4
    block image, synced back as a diff the user approves; (b) overlay rootfs + rsync over vsock.
    Spike decides.
  - Boot target <200 ms; one VM per session, reused across tool calls.
  - Fallback where KVM is missing (macOS, CI): bubblewrap (`/usr/bin/bwrap` present) or no sandbox.

## 7. Context & config

- `~/.codecraft/config.toml` + project `.codecraft/config.toml`. Providers, models, approval mode, limits.
- Instructions: `~/.codecraft/AGENTS.md`, then walk from git root down to cwd; per dir first of
  `AGENTS.override.md, AGENTS.md, CLAUDE.md`; 32 KB total cap. (codex + pi)
- System prompt = named sections (preamble, tools, rules, project context, cwd/date). Stable prefix for caching.
- Skills: `SKILL.md` dirs listed by name+description only; model reads on demand. (pi) — v1.

## 8. Sessions (SQLite)

- One DB: `~/.codecraft/codecraft.db` (WAL mode). Source of truth for all sessions.
- Tables:
  - `sessions(id, parent_session_id, cwd, title, model, created_at, updated_at)` — `parent_session_id` links sub-agent sessions.
  - `entries(id, session_id, parent_id, kind, data_json, created_at)` — append-only; `parent_id` makes a tree (branching). Kinds: `message`, `model_change`, `compaction`, `usage`.
  - `approvals(project, rule, decision)` — saved "always allow" decisions.
  - `entries_fts` (FTS5) — search across past sessions.
- Each tool call is written before it runs; orphaned calls on restart are marked failed, never replayed. (opencode)
- `codecraft resume [id]`, `codecraft -c` (continue last), `codecraft sessions search <q>`, `codecraft export <id>` → JSONL.

## 8b. Sub-agents

- `task` tool: `{agent, prompt, description}` → spawns a child session (own context, own `CancellationToken`)
  in the same engine, runs to completion, returns its final message (+ short tool summary) to the parent.
- Multiple `task` calls in one turn run in parallel (they take the read lock).
- Agent definitions: Markdown + YAML frontmatter (`name, description, model?, tools?`) in
  `~/.codecraft/agents/` and `.codecraft/agents/`. Built-ins: `general` (all tools), `explore` (read-only).
- Depth limit 1 by default (sub-agents can't spawn sub-agents); configurable.
- Child approvals bubble up to the parent's TUI; child sessions are browsable via `parent_session_id`.
- With the Claude bridge, `task` is exposed over MCP like any other tool, so it works there too.

## 9. TUI

- Inline viewport (not alt-screen): committed history → terminal scrollback; only live cell + composer redraw.
- Streaming markdown committed at newline boundaries, rendered via pulldown-cmark.
- Composer: multiline, history, `@file` fuzzy picker, slash commands (`/model`, `/compact`, `/clear`, `/resume`).
- Esc interrupts; Enter while running = steer, Alt+Enter = follow-up.

## Status (2026-11-19)

| Milestone | State | Verified by |
|---|---|---|
| M1 headless core | done | Ollama gemma4:e2b fixed a bug via read → edit → bash in `-p` mode |
| M2 sub-agents | done | claude/haiku delegated to `explore` (glob + reads), then edited |
| M3 claude bridge | done | stream-json + `mcp-serve`; interrupt kills and `--resume`s claude |
| M4 TUI | done | tmux: streaming, approvals (y/a/n), Esc interrupt, /quit + resume |
| M5 SQLite + compaction | done | cross-provider resume (claude → ollama); forced auto-compaction |
| M6 Firecracker | spike done, wired in | `--sandbox`: python ran in VM, result synced back, curl had no network |
| M7 polish | not started | Responses API, apply_patch, MCP client, model catalog |

Deviations from the plan:
- Tools run after the stream finishes, not while it streams.
- The bridge's `mcp-serve` handles tool calls one at a time (parallel calls from claude serialize).
- Sandbox is one VM per command (~2 s boot + workspace copy). Next: persistent VM per session
  with a vsock guest agent, and approve-the-diff before sync-back.
- No steer/follow-up for the claude bridge mid-turn (queued until claude's turn ends).

## 10. Build order (milestones)

1. **M1 — headless core** (~1 day): protocol, Anthropic + OpenAI-compatible adapters, loop, read/write/edit/bash/grep/glob, `codecraft -p "..."`. Test against a real model.
2. **M2 — sub-agents** (~½ day): `task` tool, child sessions (in-memory until M5), parallel runs, `general` + `explore` built-ins.
3. **M3 — claude bridge** (~½ day): `mcp-serve` + stream-json provider.
4. **M4 — TUI** (~1 day): inline viewport, streaming markdown, approvals, composer.
5. **M5 — SQLite store + compaction** (~1 day).
6. **M6 — Firecracker sandbox spike** (~1–2 days).
7. **M7 — polish**: Responses API, apply_patch, catalog, MCP client.

## Not doing (v0)

180-crate split, app-server JSON-RPC, Starlark exec policy, plugins/extensions API (deferred),
voice (deferred), cloud tasks, multi-agent mailbox, subscription OAuth spoofing.
