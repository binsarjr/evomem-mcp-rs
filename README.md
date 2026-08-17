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

## Project setup for coding harnesses

The installer configures one project only. Every selected harness sends the
same namespace header, so they share one project brain while unrelated projects
do not load it. Existing config is merged, not replaced.

Examples use `http://localhost:8080/mcp` and namespace `personal`. Replace the
URL when the server runs on another machine. Use the same namespace in every
harness that should share a brain.

### Step 1: install the binary

```bash
cargo install --git https://github.com/binsarjr/evomem-mcp-rs
```

The same binary runs the MCP server and installs project integration. No Python
runtime or manually maintained hook script is required.

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

`--client all` is the default and configures supported harnesses detected from
`PATH`, standard editor locations, or existing project markers. Force one with
`--client codex`, `claude-code`, `opencode`, `gemini-cli`, `cursor`, or
`roo-code`. Add `--dry-run` to preview without writing. Re-running setup is
idempotent.

Fasticket workspace example:

```bash
evomem-mcp-rs setup \
  --project /Users/user/Workspaces/fasticket/fasticket-workspaces \
  --url http://raspberrypi.local:8090/mcp \
  --namespace fasticket-dev
```

All adapters use the canonical `.agents/skills/evomem-memory` skill and the
same `X-Evomem-Namespace` value:

| Harness | Project MCP config | Compaction behavior |
| --- | --- | --- |
| Codex | `.codex/config.toml` | Binary `SessionStart` hook |
| Claude Code | `.mcp.json` | Binary `SessionStart` hook |
| OpenCode | `opencode.json` | Auto-loaded project plugin preserves a checkpoint action |
| Gemini CLI | `.gemini/settings.json` | MCP + skill; all memory tools remain interactive |
| Cursor | `.cursor/mcp.json` | MCP + skill; `preCompact` is observational only |
| Roo Code | `.roo/mcp.json` | MCP + skill; no lifecycle hook is installed |

Codex, Claude Code, OpenCode, and Roo Code pre-approve only `memory_recall` and
`memory_remember`; `memory_forget` remains interactive. Gemini leaves all three
interactive, and Cursor uses its normal MCP approval UI. Existing changed files
receive a sibling `*.evomem.bak` backup. For a non-Git Codex workspace root,
setup adds `.codex` to the user-level project-root markers; the MCP URL and
namespace still remain project-only.

Restart each harness from the project root and trust project files when asked.
Check its MCP and skills UI before using memory.

Official client references: [OpenCode](https://dev.opencode.ai/docs/mcp-servers/),
[Gemini CLI](https://geminicli.com/docs/tools/mcp-server/),
[Cursor](https://cursor.com/docs/mcp), and
[Roo Code](https://roocodeinc.github.io/Roo-Code/features/mcp/using-mcp-in-roo/).
The setup command currently targets macOS and Linux; Claude's compatibility
skill uses a project symlink.

### Step 4: verify the integrations

```bash
printf '%s\n' '{"hook_event_name":"SessionStart","source":"compact"}' | \
  evomem-mcp-rs hook compact
```

The command prints the Codex/Claude-compatible hook output. Then ask one
harness to remember a harmless unique fact and another to recall it. Run
`/compact` in Codex, Claude Code, or OpenCode to exercise its installed
compaction integration.

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

#### Connect OpenCode to the same project

Create `<project-root>/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "evomem": {
      "type": "remote",
      "url": "http://localhost:8080/mcp",
      "enabled": true,
      "headers": {
        "X-Evomem-Namespace": "personal"
      }
    }
  },
  "permission": {
    "evomem_memory_recall": "allow",
    "evomem_memory_remember": "allow",
    "evomem_memory_forget": "ask"
  }
}
```

The installer also creates `.opencode/plugins/evomem-memory.js`. OpenCode loads
project plugins automatically; no npm install is needed. The plugin adds a
checkpoint action to the compacted continuation context. Verify with
`opencode mcp list` and `opencode debug skill`.

#### Connect Gemini CLI to the same project

Create `<project-root>/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "evomem": {
      "httpUrl": "http://localhost:8080/mcp",
      "headers": {
        "X-Evomem-Namespace": "personal"
      },
      "trust": false
    }
  }
}
```

Gemini discovers the shared `.agents/skills` directory. Add the managed trigger
shown below to `GEMINI.md`, restart Gemini, then verify with `/mcp` and
`/skills list`. All three tools intentionally retain Gemini's normal approval
prompt because workspace-level fine-grained allow policies are not reliable.

#### Connect Cursor to the same project

Create `<project-root>/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "evomem": {
      "url": "http://localhost:8080/mcp",
      "headers": {
        "X-Evomem-Namespace": "personal"
      }
    }
  }
}
```

Cursor discovers `.agents/skills` and root `AGENTS.md`. Reload the workspace,
then inspect **Settings > Tools & MCP**. Setup does not install a Cursor
`preCompact` hook because Cursor documents it as observational and its output
cannot inject model context.

#### Connect Roo Code to the same project

Create `<project-root>/.roo/mcp.json`:

```json
{
  "mcpServers": {
    "evomem": {
      "type": "streamable-http",
      "url": "http://localhost:8080/mcp",
      "headers": {
        "X-Evomem-Namespace": "personal"
      },
      "alwaysAllow": ["memory_recall", "memory_remember"],
      "disabled": false
    }
  }
}
```

Roo Code discovers both `.agents/skills` and root `AGENTS.md`. Reload the
workspace and inspect the Roo MCP panel. `memory_forget` is deliberately absent
from `alwaysAllow`.

#### Cline limitation

Cline is not included in `--client all`. Current Cline releases resolve MCP
settings from user/global storage (or `CLINE_MCP_SETTINGS_PATH`) rather than a
native project MCP file. Setup will not weaken project isolation by writing a
global exception. Cline can be added when it supports project-scoped MCP
configuration.

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

Codex, OpenCode, Gemini CLI, Cursor, and Roo Code discover the project copy
under `.agents/skills`; Claude Code follows the project symlink under
`.claude/skills`. If the Claude path already exists, keep one canonical copy
and replace the duplicate only after reviewing it.

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

For OpenCode, create `.opencode/plugins/evomem-memory.js` instead:

```js
export const EvomemMemoryPlugin = async () => ({
  "experimental.session.compacting": async (_input, output) => {
    output.context.push(
      "After compaction, activate evomem-memory and checkpoint verified durable facts before continuing."
    );
  },
});
```

Gemini CLI, Cursor, and Roo Code do not currently expose a safe equivalent that
can inject a post-compaction instruction, so their adapters rely on the shared
skill and project trigger at session start and verified milestones.

#### Add the project trigger

Add this short block to `<project-root>/AGENTS.md`, `<project-root>/CLAUDE.md`,
and `<project-root>/GEMINI.md` for the harnesses that use each file. Keep the
full workflow in the skill instead of duplicating it in instruction files.

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

Then start fresh harness sessions:

1. Inspect MCP status: `/mcp` in Codex/Claude/Gemini, `opencode mcp list`,
   Cursor's **Tools & MCP**, or Roo's MCP panel.
2. Inspect the `evomem-memory` skill in the harness's skills UI.
3. Review project trust prompts. Confirm recall/remember approval behavior and
   keep `memory_forget` interactive.
4. Ask one harness to remember a harmless unique test fact.
5. Ask another harness to recall it, proving both use the same namespace.
6. Repeat the checkpoint; no duplicate memory should be written.
7. Run `/compact` in Codex, Claude Code, or OpenCode and confirm the checkpoint
   instruction survives compaction.

### Troubleshooting and rollback

- If the MCP connection fails, verify the URL, server process, firewall, and
  `EVOMEM_ALLOWED_HOSTS` value.
- If a hook reports exit 127, rerun setup after reinstalling the binary so the
  hook receives its current absolute path.
- If the skill is absent, restart the harness and inspect its skills UI; make
  sure the canonical `.agents/skills/evomem-memory/SKILL.md` exists and the
  Claude symlink resolves to it.
- If `--client all` skips an installed editor extension, rerun with its explicit
  selector, for example `--client roo-code`.
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
