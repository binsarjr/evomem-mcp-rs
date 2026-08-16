# evomem-mcp-rs

MCP server (Streamable HTTP) yang **menanamkan [evomem](https://github.com/anvie/evomem) sebagai library Rust**, bukan lewat subprocess atau REST proxy. Hasilnya satu binary statis: memory terpisah, dipakai via MCP.

> evomem dipin pada `tag = "v0.4.2"`.

## Konsep

```
┌──────────────┐  MCP (Streamable HTTP /mcp)   ┌─────────────────────┐
│ MCP client   │ ─────────────────────────────▶ │ evomem-mcp-rs       │
│ (Claude/     │   tools: memory_search/        │ (1 binary Rust)     │
│  Cursor/agent│   memory_think/memory_graph/   │  evomem sebagai     │
│  kamu)       │   memory_capture/...           │  library (in-memory)│
└──────────────┘                                └──────────┬──────────┘
                                                           │
                                              EVOMEM_ROOT/<namespace>/
                                              ├── default/
                                              │   ├── *.md
                                              │   └── .evomem.db
                                              ├── agent-alice/
                                              └── user-123/
```

- **Multi-namespace** — satu server, banyak brain terpisah. Namespace ditentukan oleh **klien lewat header `X-Evomem-Namespace`** (bukan arg tool); folder brain = `<EVOMEM_ROOT>/<namespace>/`.
- **Disk = source of truth** — pengetahuan adalah file `.md`; `.evomem.db` dibangun ulang dari `sync`.
- **Deterministik & tanpa LLM saat retrieval** — lexical + hash-vector + knowledge graph.

## Cara pakai

### 1. Jalankan langsung (dev)

```bash
EVOMEM_ROOT=./vault BIND=0.0.0.0:8080 cargo run --release
```

### 2. Docker Compose

```bash
docker compose up --build
```

Folder `./vault` di host di-mount ke `/vault` di container; tiap namespace menjadi subfolder di sana.

## Environment variables

| Var | Default | Keterangan |
|---|---|---|
| `EVOMEM_ROOT` | `./vault` | Direktori induk semua brain (1 namespace = 1 subfolder). |
| `EVOMEM_DEFAULT_NAMESPACE` | `default` | Namespace fallback jika klien tidak mengirim header `X-Evomem-Namespace`. |
| `EVOMEM_ALLOWED_HOSTS` | loopback only | Allowlist header `Host` (dipisah koma), atau `*` untuk semua. Default rmcp menolak Host selain `localhost/127.0.0.1/::1`. |
| `BIND` | `0.0.0.0:8080` | Alamat bind endpoint MCP. |

## Tools

Tool **tidak punya arg `namespace`**. Namespace selalu diambil dari header `X-Evomem-Namespace`, sehingga agent tidak bisa (dan tidak perlu) memilih namespace sendiri — knowledge tiap agent tidak bercampur.

| Tool | Fungsi | Input utama |
|---|---|---|
| `memory_init` | Pastikan brain namespace ada (idempoten). | — |
| `memory_sync` | Re-index file `.md` → `.evomem.db`. | — |
| `memory_search` | Hybrid retrieval (lexical+vector+graph). | `query`, `mode`, `limit`, `min_score` |
| `memory_think` | Sintesis + gap analysis dengan citation. | `query`, `mode` |
| `memory_graph` | Telusuri knowledge graph bertipe (multi-hop). | `start`, `edge`, `hops` |
| `memory_capture` | Catat fakta cepat ke `inbox/`. | `text`, `title`, `tags` |
| `memory_get_doc` | Baca isi penuh satu dokumen. | `slug` |
| `memory_forget` | Hapus (soft-delete) satu dokumen, lalu re-sync. | `slug` |
| `memory_stats` | Statistik knowledge store. | — |
| `memory_list_namespaces` | Daftar semua brain. | — |

`memory_capture` menerima `tags` (array, opsional, 1–8, lowercase `[a-z0-9_-]`),
default `["captured"]` bila kosong. Tulis `[[Name]]` di dalam `text` untuk
membangun edge knowledge graph; edge bertipe diinferensi dari kalimat berbahasa
Inggris ("works at", "founded", "advises", "attended", "invested in", fallback
`mentions`). Hapus dokumen dengan `memory_forget` (hapus `.md` + re-sync →
soft-delete).

## Registrasi di klien MCP

Namespace di-pin di sisi klien via header. Satu server, banyak klien — tiap klien/agent menunjuk namespace-nya sendiri:

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

Agent yang terkoneksi sebagai `evomem-alpha` akan selalu membaca/menulis `workspace-alpha`, dan `evomem-beta` ke `workspace-beta` — walau sama-sama memanggil `memory_search`/`memory_capture` tanpa arg `namespace`.

### Cursor

Konfigurasi MCP di Cursor (`.cursor/mcp.json`) juga mendukung header:

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

> Untuk produksi, taruh di belakang reverse proxy + TLS dan tambahkan auth token di layer proxy (server ini sendiri belum melakukan autentikasi).

> **Catatan isolasi:** ini bukan mekanisme keamanan (header bisa diubah manual di config klien). Tujuannya adalah **mem-pin namespace di level konfigurasi klien**, bukan di argumen agent, sehingga agent tidak "nyasar" memakai brain lain. Kalau butuh isolasi keras (anti-spoof), tambahkan auth token di layer proxy.

## Menulis pengetahuan ke brain

Brain adalah folder markdown. Contoh doc dengan inline `[[wiki-link]]` (membangun knowledge graph):

```markdown
---
title: "Budi Santoso"
type: person
tags: [founder]
aliases: [budi]
---

[[Budi Santoso]] mendirikan [[Nuwaira]] dan bekerja di [[Acme Corp]].
```

Setelah file ditulis, panggil `memory_sync` (atau biarkan `memory_capture` yang langsung index).

## Catatan teknis

- `Store` evomem membungkus `rusqlite::Connection` (Send, bukan Sync), jadi setiap store dijaga `Arc<Mutex<Store>>` dan semua kerja SQLite dijalankan di `tokio::task::spawn_blocking` — pola yang sama dengan server REST bawaan evomem (`src/server/mod.rs`).
- `HashEmbedder` (BLAKE3/feature-hash, 512-dim) adalah embedder default; `id()`-nya wajib konsisten (`hash-v1-d512`) atau `Store::open` akan menolak (`EmbedderMismatch`).
- Semua error tool dikembalikan sebagai **tool-level error** (string), sehingga pesannya terlihat oleh agent.
