# evomem-mcp-rs

MCP server (Streamable HTTP) that **embeds [evomem](https://github.com/anvie/evomem) as a Rust library** — not via subprocess or a REST proxy. The result is a single static binary: isolated memory, accessed over MCP.

> evomem is pinned at `tag = "v0.4.2"`.

## Concepts

```
┌──────────────┐  MCP (Streamable HTTP /mcp)   ┌─────────────────────┐
│ MCP client   │ ─────────────────────────────▶ │ evomem-mcp-rs       │
│ (Claude/     │   tools: memory_remember/      │ (1 binary Rust)     │
│  Cursor/agent│   memory_recall/               │  evomem embedded    │
│  you)        │   memory_forget                │  as a library       │
└──────────────┘                                └──────────┬──────────┘
                                                           │
                                              EVOMEM_ROOT/<namespace>/
                                              ├── default/
                                              │   ├── *.md
                                              │   └── .evomem.db
                                              ├── agent-alice/
                                              └── user-123/
```

- **Multi-namespace** — one server, many isolated brains. The namespace is chosen by the **client via the `X-Evomem-Namespace` header** (not a tool argument); the brain folder is `<EVOMEM_ROOT>/<namespace>/`.
- **Disk is the source of truth** — knowledge lives in `.md` files; `.evomem.db` is rebuilt from disk.
- **Deterministic, no LLM at retrieval time** — lexical + hash-vector + knowledge graph.

## Usage

### 1. Run directly (dev)

```bash
EVOMEM_ROOT=./vault BIND=0.0.0.0:8080 cargo run --release
```

### 2. Docker Compose

```bash
docker compose up --build
```

The host `./vault` is mounted at `/vault` in the container; each namespace becomes a subfolder there.

## Environment variables

| Var | Default | Description |
|---|---|---|
| `EVOMEM_ROOT` | `./vault` | Parent directory of all brains (1 namespace = 1 subfolder). |
| `EVOMEM_DEFAULT_NAMESPACE` | `default` | Fallback namespace when the client sends no `X-Evomem-Namespace` header. |
| `EVOMEM_DEFAULT_AUTHOR` | `inbox` | Fallback author folder when the client sends no `X-Evomem-Author` header. |
| `EVOMEM_ALLOWED_HOSTS` | loopback only | Comma-separated `Host` header allowlist, or `*` for all. rmcp rejects non-`localhost/127.0.0.1/::1` hosts by default. |
| `BIND` | `0.0.0.0:8080` | Bind address of the MCP endpoint. |

## Tools

The server exposes a lean, three-tool surface. Tools **have no `namespace`
argument** — the namespace is always taken from the `X-Evomem-Namespace` header,
so an agent cannot (and need not) choose a namespace itself; each agent's
knowledge stays separate.

| Tool | Purpose | Main input |
|---|---|---|
| `memory_remember` | Remember a durable fact into long-term memory (indexed immediately). | `text`, `title`, `tags` |
| `memory_recall` | Recall from memory: `search` (hybrid lookup) \| `think` (synthesis + gaps) \| `graph` (traverse). | `query`, `mode`, `edge`, `hops` |
| `memory_forget` | Soft-delete one document, then re-sync. | `slug` |

`memory_remember` accepts `tags` (array, optional, 1–8, lowercase `[a-z0-9_-]`),
defaulting to `["captured"]` when empty. Wrap entity names in `[[Name]]` inside
`text` to build knowledge-graph edges; edge types are inferred from English
sentences ("works at", "founded", "advises", "attended", "invested in", fallback
`mentions`).

`memory_recall`'s `mode` is `search` (default) | `think` | `graph`. `edge` and
`hops` apply only to `graph` mode (where `query` is the start entity).

`memory_forget` removes the document's `.md` file and re-indexes (soft-delete).

## Codex and Claude Code setup

The installer configures one project only: Codex and Claude Code share the
chosen namespace, while unrelated projects do not load Evomem. It safely merges
existing config, installs the shared memory skill, and adds a binary lifecycle
hook that asks the model to checkpoint only verified durable facts.

Examples use `http://localhost:8080/mcp` and namespace `personal`. Replace the
URL when the server runs on another machine. Use the same namespace in both
clients to share a brain.

### Step 1: install the binary

```bash
cargo install --git https://github.com/binsarjr/evomem-mcp-rs
```

The same binary runs the MCP server and installs project integration. No Python
runtime or separate hook script is required.

### Step 2: start or identify the server

From this repository:

```bash
EVOMEM_ROOT=./vault BIND=0.0.0.0:8080 cargo run --release
```

For Docker, run `docker compose up --build` instead. When connecting through a
hostname such as `raspberrypi.local`, include it in `EVOMEM_ALLOWED_HOSTS`.

### Step 3: configure one project

```bash
evomem-mcp-rs setup \
  --project /absolute/path/to/project \
  --url http://localhost:8080/mcp \
  --namespace personal
```

`--client all` is the default. Use `--client codex` or
`--client claude-code` to target one client, and add `--dry-run` to preview
without writing. Run the same command again after an upgrade; it is idempotent
and does not duplicate hooks, permissions, or instruction blocks.

The installer creates or merges:

- `.codex/config.toml` and `.codex/hooks.json`
- `.mcp.json` and `.claude/settings.local.json`
- `.agents/skills/evomem-memory` and Claude's project skill link
- managed Evomem blocks in `AGENTS.md` and `CLAUDE.md`

Only `memory_recall` and `memory_remember` are pre-approved.
`memory_forget` remains interactive. Existing changed files receive a sibling
`*.evomem.bak` backup. For a non-Git workspace root, the installer also adds
`.codex` to the user-level Codex project-root markers; the MCP URL and namespace
still remain project-only.

Restart each client from the project root, then trust the project's MCP server
and hook when prompted. Verify with `/mcp`; Claude Code should also list
`/evomem-memory` under `/skills`.

### Step 4: verify the hook

```bash
printf '%s\n' '{"hook_event_name":"SessionStart","source":"compact"}' | \
  evomem-mcp-rs hook compact
```

The command prints a Codex-compatible `hookSpecificOutput` object. Then ask one
client to remember a harmless unique fact and the other to recall it. Run
`/compact` to exercise the installed lifecycle hook.

### Manual setup (fallback)

The installer is the supported path. The following files document its exact
project-scoped configuration for environments where the binary cannot write
the project.

#### Connect Codex to one project

Create `<project-root>/.codex/config.toml`:

```toml
[mcp_servers.evomem]
url = "http://localhost:8080/mcp"
http_headers = { "X-Evomem-Namespace" = "personal" }

[mcp_servers.evomem.tools.memory_recall]
approval_mode = "approve"

[mcp_servers.evomem.tools.memory_remember]
approval_mode = "approve"
```

The two per-tool approvals let proactive recall and checkpoints run without
stopping for confirmation. `memory_forget` intentionally remains interactive.

If the project root is not itself a Git repository (for example, a workspace
that contains several child repositories), add this discovery setting to the
user-level `~/.codex/config.toml`:

```toml
project_root_markers = [".git", ".codex"]
```

This changes project-root discovery only; the MCP URL and namespace remain in
the project config. Start Codex from that project root and use `/mcp` to verify
the connection. For headless checks in a non-Git workspace, add
`--skip-git-repo-check` to `codex exec`.

#### Connect Claude Code to the same project

From the project root, register the same URL and namespace at project scope:

```bash
claude mcp add-json --scope project evomem \
  '{"type":"http","url":"http://localhost:8080/mcp","headers":{"X-Evomem-Namespace":"personal"},"alwaysLoad":true}'
claude mcp get evomem
```

This creates `<project-root>/.mcp.json`. Approve the project MCP when Claude
Code asks on first launch. `alwaysLoad` keeps all three memory tools available
from the first turn and requires Claude Code 2.1.121 or newer. Inside Claude
Code, use `/mcp` to inspect connection health.

#### Install the shared memory skill

From this repository, set the target project and copy the skill there:

```bash
EVOMEM_TARGET_PROJECT=/absolute/path/to/project
mkdir -p "$EVOMEM_TARGET_PROJECT/.agents/skills/evomem-memory" \
  "$EVOMEM_TARGET_PROJECT/.claude/skills"
cp -R integrations/agent-memory/evomem-memory/. \
  "$EVOMEM_TARGET_PROJECT/.agents/skills/evomem-memory/"
ln -s ../../.agents/skills/evomem-memory \
  "$EVOMEM_TARGET_PROJECT/.claude/skills/evomem-memory"
```

Codex discovers the project copy under `.agents/skills`; Claude Code follows
the project symlink under `.claude/skills`. If the Claude path already exists,
keep one canonical copy and replace the duplicate only after reviewing it.

The skill recalls prior work before overlapping tasks and checkpoints facts at
durable milestones: agreed decisions, verified root causes or fixes, tested
configuration, stable preferences, important paths/commands, and unresolved
blockers. It skips hypotheses, transient progress, duplicates, secrets, and raw
logs.

#### Add the compact checkpoint hook

Create or merge `<project-root>/.codex/hooks.json`. Replace
`/absolute/path/to/evomem-mcp-rs` with the installed binary path returned by
`command -v evomem-mcp-rs`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "^compact$",
        "hooks": [
          {
            "type": "command",
            "command": "'/absolute/path/to/evomem-mcp-rs' hook compact",
            "timeout": 5,
            "statusMessage": "Checkpointing durable memory"
          }
        ]
      }
    ]
  }
}
```

Merge the equivalent entry into `<project-root>/.claude/settings.json` for a
shared project configuration, or `.claude/settings.local.json` for a local-only
installation. Do not replace existing settings or hooks:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "compact",
        "hooks": [
          {
            "type": "command",
            "command": "'/absolute/path/to/evomem-mcp-rs' hook compact",
            "timeout": 5
          }
        ]
      }
    ]
  },
  "permissions": {
    "allow": [
      "mcp__evomem__memory_recall",
      "mcp__evomem__memory_remember"
    ]
  }
}
```

Both clients emit `SessionStart` with source `compact` after manual or automatic
compaction. The hook adds a short model instruction; the LLM then performs
`memory_recall`, evaluates candidate facts, calls `memory_remember` only when
needed, and recalls the active task before continuing.

#### Add the project trigger

Add this short block to `<project-root>/AGENTS.md` and
`<project-root>/CLAUDE.md`. Keep the full workflow in the skill instead of
duplicating it in both instruction files.

```markdown
<!-- evomem-mcp-rs:start -->
## Evomem long-term memory

Use the `evomem-memory` skill for non-trivial work that may overlap prior
sessions. Recall before asking the user to repeat context, and checkpoint new
durable facts at verified milestones. Never save secrets or raw transcripts.
<!-- evomem-mcp-rs:end -->
```

The server also advertises the baseline policy through MCP `instructions`.

#### Verify the complete setup

From the project root, validate the installed files:

```bash
printf '%s\n' '{"hook_event_name":"SessionStart","source":"compact"}' | \
  evomem-mcp-rs hook compact
```

Then start fresh Codex and Claude Code sessions:

1. Check `/mcp`; Claude should also list `/evomem-memory` under `/skills`.
2. Review and trust the project MCP and project hook when each client asks.
3. Confirm only `memory_recall` and `memory_remember` are pre-approved; keep
   `memory_forget` as a per-call approval.
4. Ask Codex to remember a harmless unique test fact.
5. Ask Claude to recall that fact, proving both use the same namespace.
6. Repeat the same checkpoint; no duplicate memory should be written.
7. Run `/compact`, then inspect `/hooks` and confirm the model performs a
   checkpoint before continuing.

### Troubleshooting and rollback

- If the MCP connection fails, verify the URL, server process, firewall, and
  `EVOMEM_ALLOWED_HOSTS` value.
- If a hook reports exit 127, rerun setup after reinstalling the binary so the
  hook receives its current absolute path.
- If the skill is absent, restart the client and inspect `/skills`; make sure
  the Claude symlink resolves to the installed Codex skill.
- If memories duplicate, recall the exact topic before each remember and keep
  one independent fact per note.
- To roll back, restore the relevant `*.evomem.bak` files, then remove newly
  created Evomem entries/files. Leave global and unrelated project configuration
  untouched.

### Claude Desktop

One server can also expose separate namespaces to different desktop agents.
For example, in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "evomem-alpha": {
      "url": "http://localhost:8080/mcp",
      "headers": { "X-Evomem-Namespace": "workspace-alpha" }
    },
    "evomem-beta": {
      "url": "http://localhost:8080/mcp",
      "headers": { "X-Evomem-Namespace": "workspace-beta" }
    }
  }
}
```

An agent connected as `evomem-alpha` always reads/writes `workspace-alpha`, and `evomem-beta` reads/writes `workspace-beta` — even though both call `memory_remember`/`memory_recall` with no `namespace` argument.

### Cursor

Cursor's MCP config (`.cursor/mcp.json`) also supports headers:

```json
{
  "mcpServers": {
    "evomem-alpha": {
      "url": "http://localhost:8080/mcp",
      "headers": { "X-Evomem-Namespace": "workspace-alpha" }
    }
  }
}
```

> For production, put it behind a reverse proxy + TLS and add an auth token at the proxy layer (this server does not authenticate on its own).

> **Isolation note:** this is not a security mechanism (the header can be edited by hand in the client config). Its purpose is to **pin the namespace at the client-config level**, not in an agent argument, so an agent can't accidentally use another brain. If you need hard isolation (anti-spoofing), add an auth token at the proxy layer.

## Team memory (multiple authors per namespace)

One namespace can be a **shared team brain**: each member writes to their own
folder, while `memory_recall` searches the whole namespace.

- `memory_remember` and `memory_forget` are scoped to the caller's **author**
  folder, chosen client-side via the `X-Evomem-Author` header (never a tool
  argument). The folder becomes the document's evomem `source_dir`.
- `memory_recall` is **not** author-scoped — it always searches the entire
  namespace, so any member can recall the whole team's knowledge.
- `memory_forget` is fail-closed: a member can only forget documents in their
  own folder (a cross-author forget returns an error).

```json
{
  "mcpServers": {
    "evomem-team": {
      "url": "http://localhost:8080/mcp",
      "headers": {
        "X-Evomem-Namespace": "team-project",
        "X-Evomem-Author": "binsar"
      }
    }
  }
}
```

Author names are normalized to a safe folder segment (`a-z0-9_-`, lowercased).
Reserved names `test` and `attachments` are rejected because evomem
hard-excludes them from recall. Leave the header unset for the single-user
default (`inbox`).

> Team isolation is the namespace boundary: recall is global **within** a
> namespace and never crosses into another namespace.

## Writing knowledge to a brain

A brain is a markdown folder. Example doc with inline `[[wiki-link]]` (builds the knowledge graph):

```markdown
---
title: "Alice"
type: person
tags: [founder]
aliases: [alice]
---

[[Alice]] founded [[Nuwaira]] and works at [[Acme Corp]].
```

`memory_remember` writes documents like this and indexes them immediately.

## Technical notes

- evomem's `Store` wraps a `rusqlite::Connection` (Send, not Sync), so each store is held in an `Arc<Mutex<Store>>` and all SQLite work runs on `tokio::task::spawn_blocking` — the same pattern as evomem's built-in REST server (`src/server/mod.rs`).
- `HashEmbedder` (BLAKE3/feature-hash, 512-dim) is the default embedder; its `id()` must stay consistent (`hash-v1-d512`) or `Store::open` rejects it (`EmbedderMismatch`).
- All tool errors are returned as **tool-level errors** (strings), so the message is visible to the agent.
