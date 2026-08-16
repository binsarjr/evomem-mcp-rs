# evomem-mcp-rs

MCP server (Streamable HTTP) that **embeds [evomem](https://github.com/anvie/evomem) as a Rust library** — not via subprocess or a REST proxy. The result is a single static binary: isolated memory, accessed over MCP.

> evomem is pinned at `tag = "v0.4.2"`.

## Concepts

```
┌──────────────┐  MCP (Streamable HTTP /mcp)   ┌─────────────────────┐
│ MCP client   │ ─────────────────────────────▶ │ evomem-mcp-rs       │
│ (Claude/     │   tools: memory_search/        │ (1 binary Rust)     │
│  Cursor/agent│   memory_think/memory_graph/   │  evomem embedded    │
│  you)        │   memory_capture/...           │  as a library       │
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
- **Disk is the source of truth** — knowledge lives in `.md` files; `.evomem.db` is rebuilt from `sync`.
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
| `EVOMEM_ALLOWED_HOSTS` | loopback only | Comma-separated `Host` header allowlist, or `*` for all. rmcp rejects non-`localhost/127.0.0.1/::1` hosts by default. |
| `BIND` | `0.0.0.0:8080` | Bind address of the MCP endpoint. |

## Tools

Tools **have no `namespace` argument**. The namespace is always taken from the `X-Evomem-Namespace` header, so an agent cannot (and need not) choose a namespace itself — each agent's knowledge stays separate.

| Tool | Purpose | Main input |
|---|---|---|
| `memory_init` | Ensure the namespace brain exists (idempotent). | — |
| `memory_sync` | Re-index `.md` files → `.evomem.db`. | — |
| `memory_search` | Hybrid retrieval (lexical+vector+graph). | `query`, `mode`, `limit`, `min_score` |
| `memory_think` | Synthesis + gap analysis with citations. | `query`, `mode` |
| `memory_graph` | Traverse the typed knowledge graph (multi-hop). | `start`, `edge`, `hops` |
| `memory_capture` | Capture a quick fact into `inbox/`. | `text`, `title`, `tags` |
| `memory_get_doc` | Read one document's full content. | `slug` |
| `memory_forget` | Soft-delete one document, then re-sync. | `slug` |
| `memory_stats` | Knowledge store statistics. | — |
| `memory_list_namespaces` | List all brains. | — |

`memory_capture` accepts `tags` (array, optional, 1–8, lowercase `[a-z0-9_-]`),
defaulting to `["captured"]` when empty. Write `[[Name]]` inside `text` to build
knowledge-graph edges; edge types are inferred from English sentences ("works
at", "founded", "advises", "attended", "invested in", fallback `mentions`).
Delete a document with `memory_forget` (remove `.md` + re-sync → soft-delete).

## Registering with MCP clients

The namespace is pinned client-side via a header. One server, many clients — each client/agent points at its own namespace:

`claude_desktop_config.json`:

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

An agent connected as `evomem-alpha` always reads/writes `workspace-alpha`, and `evomem-beta` reads/writes `workspace-beta` — even though both call `memory_search`/`memory_capture` with no `namespace` argument.

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

After writing the file, call `memory_sync` (or let `memory_capture` index immediately).

## Technical notes

- evomem's `Store` wraps a `rusqlite::Connection` (Send, not Sync), so each store is held in an `Arc<Mutex<Store>>` and all SQLite work runs on `tokio::task::spawn_blocking` — the same pattern as evomem's built-in REST server (`src/server/mod.rs`).
- `HashEmbedder` (BLAKE3/feature-hash, 512-dim) is the default embedder; its `id()` must stay consistent (`hash-v1-d512`) or `Store::open` rejects it (`EmbedderMismatch`).
- All tool errors are returned as **tool-level errors** (strings), so the message is visible to the agent.
