# codecraft

A terminal coding agent in Rust. Small core (after pi), multi-provider with
Claude via your own `claude` login (after opencode), engine shape and
safety ideas from codex. Design: [docs/design.md](docs/design.md).

## Build

```sh
make build          # release → bin/codecraft
make debug          # debug   → bin/codecraft
make test
```

## Use

```sh
bin/codecraft                                   # interactive TUI
bin/codecraft -p "fix the failing test"         # print mode
bin/codecraft -p --json "..."                   # JSONL events
bin/codecraft -m claude/sonnet                  # Claude via your Claude Code login
bin/codecraft -m anthropic/claude-sonnet-4-5    # Anthropic API key
bin/codecraft -m ollama/qwen3                   # any OpenAI-compatible endpoint
bin/codecraft -c                                # continue last session here
bin/codecraft -r <id>                           # resume a session
bin/codecraft --sandbox                         # bash runs in a Firecracker microVM
bin/codecraft sessions [list|search|show|export|rm]
bin/codecraft models | agents | login claude | sandbox setup
```

TUI keys: Enter send · Ctrl+J newline · while running, Enter steers and
Alt+Enter queues a follow-up · Esc interrupts · `/help` for commands.

## Claude

codecraft never handles Claude credentials. Run `claude auth login` once;
the `claude/<model>` provider runs `claude -p` in stream-json mode with its
built-in tools off and serves codecraft's tools to it over MCP
(`codecraft mcp-serve`), so approvals, sub-agents and history stay
codecraft's. Same approach as opencode's "Claude Agent" provider.

## Config

`~/.codecraft/config.toml`, overridden by `<repo>/.codecraft/config.toml`:

```toml
model = "claude/sonnet"
approval = "ask"            # ask | auto-edit | yolo
thinking = "off"            # off | low | medium | high
sandbox = "none"            # none | firecracker
subagent_depth = 1

[providers.lmstudio]
kind = "openai-chat"        # anthropic | openai-chat | claude-cli
base_url = "http://localhost:1234/v1"
context_window = 32000
```

Instructions: `~/.codecraft/AGENTS.md`, then `AGENTS.override.md` /
`AGENTS.md` / `CLAUDE.md` from the git root down to the cwd (32 KB cap).

Sub-agents: built-in `general` and `explore`; add your own as
`~/.codecraft/agents/*.md` or `.codecraft/agents/*.md`:

```markdown
---
name: reviewer
description: Reviews a diff for bugs
tools: read, grep, glob, bash
model: claude/opus
---
You are a careful reviewer...
```

Sessions live in SQLite at `~/.codecraft/codecraft.db` (full-text
searchable, JSONL export). Set `CODECRAFT_HOME` to relocate everything.
