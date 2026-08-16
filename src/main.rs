//! evomem-mcp-rs — MCP server (Streamable HTTP) that embeds evomem as a library.
//!
//! Multi-namespace: each namespace is a separate brain directory under
//! `EVOMEM_ROOT`. The namespace is chosen by the CLIENT via the
//! `X-Evomem-Namespace` request header (falling back to
//! `EVOMEM_DEFAULT_NAMESPACE` / "default") — NOT as a tool argument. This pins
//! each agent to its own brain and keeps knowledge from mixing between agents.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use evomem::embed::{Embedder, HashEmbedder};
use evomem::error::EvoError;
use evomem::model::Mode;
use evomem::store::Store;
use evomem::{ingest, search, think};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{service::RequestContext, schemars, tool, tool_router, RoleServer};
use serde::Deserialize;
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// State
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    /// Base directory; each namespace lives at `<root>/<namespace>/`.
    root: PathBuf,
    /// Lazily-opened stores, one per namespace. `Store` wraps a rusqlite
    /// `Connection` (Send, not Sync), so each store is guarded by its own
    /// `Mutex`, and the map itself is guarded for cheap clone-on-lookup.
    stores: Arc<Mutex<HashMap<String, Arc<Mutex<Store>>>>>,
    embedder: Arc<HashEmbedder>,
}

#[derive(Clone)]
struct EvomemServer {
    state: AppState,
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Lock a mutex, recovering from poisoning (mirrors evomem's own server/mod.rs).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Validate a namespace and normalize it to a single safe path segment.
fn normalize_namespace(ns: Option<&str>) -> Result<String, String> {
    let default =
        std::env::var("EVOMEM_DEFAULT_NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let ns: &str = ns
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default.as_str());
    if ns == "." || ns == ".." || ns.contains('/') || ns.contains('\\') || ns.contains('\0') {
        return Err(format!("invalid namespace '{ns}'"));
    }
    if ns.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        return Err(format!(
            "namespace '{ns}' contains invalid characters (a-z, 0-9, '-', '_' only)"
        ));
    }
    Ok(ns.to_string())
}

fn parse_mode(s: Option<&str>) -> Mode {
    s.and_then(|m| m.parse().ok()).unwrap_or_default()
}

/// First non-empty line of `text`, capped at 8 words / 60 chars (mirrors evomem).
fn derive_title(text: &str) -> String {
    let first_line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Captured note");
    let words: Vec<&str> = first_line.split_whitespace().take(8).collect();
    let mut t = words.join(" ");
    if t.chars().count() > 60 {
        t = t.chars().take(60).collect();
    }
    if t.is_empty() {
        t = "Captured note".to_string();
    }
    t
}

/// Strip control characters (newlines included) so a user-supplied title can
/// never break the generated YAML frontmatter.
fn sanitize_title(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "Captured note".to_string()
    } else {
        collapsed
    }
}

/// Lowercase a title to a safe filename slug (mirrors evomem).
fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed
    }
}

/// Always double-quote: unquoted YAML scalars have too many sharp edges.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Normalize user tags: lowercase, `[a-z0-9_-]` only, dedupe, cap at 8.
/// Empty result falls back to `["captured"]` (evomem's default).
fn normalize_tags(tags: Option<Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in tags.unwrap_or_default() {
        let tag: String = raw
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let tag = tag.trim_matches(|c| c == '-' || c == '_').to_string();
        if tag.is_empty() || out.contains(&tag) {
            continue;
        }
        out.push(tag);
        if out.len() == 8 {
            break;
        }
    }
    if out.is_empty() {
        vec!["captured".to_string()]
    } else {
        out
    }
}

/// Header the MCP client sets to pick its namespace (configured in the client,
/// NOT passed by the agent — the agent never chooses its own namespace).
const NAMESPACE_HEADER: &str = "x-evomem-namespace";

/// Read the namespace from the request header, if present.
fn namespace_from_header(ctx: &RequestContext<RoleServer>) -> Option<String> {
    let parts = ctx.extensions.get::<axum::http::request::Parts>()?;
    let value = parts.headers.get(NAMESPACE_HEADER)?.to_str().ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

impl EvomemServer {
    /// Get (or lazily open/init) the store for a namespace.
    fn resolve_store(&self, ns: &str) -> Result<Arc<Mutex<Store>>, String> {
        {
            let map = lock(&self.state.stores);
            if let Some(s) = map.get(ns) {
                return Ok(Arc::clone(s));
            }
        }

        let brain = self.state.root.join(ns);
        let store = match Store::open(&brain, self.state.embedder.id()) {
            Ok(s) => s,
            Err(EvoError::NotInitialized(_)) => {
                let s = Store::init(&brain, self.state.embedder.id(), self.state.embedder.dim())
                    .map_err(|e| e.to_string())?;
                let _ = Store::ensure_gitignore(&brain);
                s
            }
            Err(e) => return Err(e.to_string()),
        };

        let arc = Arc::new(Mutex::new(store));
        {
            let mut map = lock(&self.state.stores);
            map.entry(ns.to_string())
                .or_insert_with(|| Arc::clone(&arc));
        }
        Ok(arc)
    }

    /// Resolve the namespace for a request: header wins, else the default.
    /// The agent cannot override it — only the client configuration can.
    fn resolve_namespace(&self, ctx: &RequestContext<RoleServer>) -> Result<String, String> {
        normalize_namespace(namespace_from_header(ctx).as_deref())
    }
}

/// Run blocking SQLite work on tokio's blocking pool, holding only the
/// per-namespace store lock (this is exactly the pattern evomem's own
/// `server/mod.rs` uses for `Store`).
async fn run_block<T, F>(
    store: Arc<Mutex<Store>>,
    embedder: Arc<HashEmbedder>,
    f: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Store, &dyn Embedder) -> Result<T, EvoError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = lock(&store);
        f(&store, embedder.as_ref())
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
    .map_err(|e| e.to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// Tool parameter structs (the MCP input schema is derived from these)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RememberParams {
    /// The fact/thought to remember.
    text: String,
    /// Optional title (derived from text if omitted).
    title: Option<String>,
    /// Optional tags (lowercase, 1-8); defaults to ["captured"].
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallParams {
    /// What to recall (entity name for graph mode).
    query: String,
    /// Recall mode: search (default) | think | graph.
    mode: Option<String>,
    /// Graph mode only: filter by edge type (works_at, founded, advises, attended, invested_in, mentions).
    edge: Option<String>,
    /// Graph mode only: traversal depth (default 2).
    hops: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ForgetParams {
    /// Document to delete: slug, title, or alias.
    slug: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Tools
// ────────────────────────────────────────────────────────────────────────────

#[tool_router(server_handler)]
impl EvomemServer {
    #[tool(description = "Remember a durable fact/thought into long-term memory (indexed immediately). Wrap entity names in [[Name]] to wire the knowledge graph (e.g. \"[[Alice]] works at [[Nuwaira]]\"); use a relation verb (\"works at\", \"founded\") for a typed edge. Pass 1-4 lowercase tags (person, project, meeting, decision, preference, ...) to categorize the note.")]
    async fn memory_remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let text = p.text;
        let title = p.title;
        let tags = p.tags;
        run_block(store, emb, move |s, e| {
            let title = sanitize_title(&title.unwrap_or_else(|| derive_title(&text)));
            let file_slug = slugify(&title);
            let now = chrono::Utc::now();
            let stamp = now.format("%Y-%m-%d-%H%M%S");
            let base_slug = format!("inbox/{stamp}-{file_slug}");

            // Same-second captures with the same title must not overwrite each other.
            let (slug, abs_path) = {
                let mut slug = base_slug.clone();
                let mut path = s.brain_root.join(format!("{slug}.md"));
                let mut n = 1;
                while path.exists() && n < 100 {
                    n += 1;
                    slug = format!("{base_slug}-{n}");
                    path = s.brain_root.join(format!("{slug}.md"));
                }
                (slug, path)
            };

            let tags = normalize_tags(tags);
            let tags_yaml = tags.iter().map(|t| yaml_quote(t)).collect::<Vec<_>>().join(", ");
            let content = format!(
                "---\ntitle: {title}\ntype: note\ncreated: {created}\ntags: [{tags}]\n---\n\n{body}\n",
                title = yaml_quote(&title),
                created = now.format("%Y-%m-%dT%H:%M:%SZ"),
                tags = tags_yaml,
                body = text.trim(),
            );

            if let Some(parent) = abs_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs_path, &content)?;

            let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
            ingest::sync_one(s, e, &slug, &content, &hash, &now.to_rfc3339(), &abs_path)?;
            s.resolve_dangling_links()?;

            Ok(serde_json::json!({
                "slug": slug,
                "path": abs_path.display().to_string(),
            }))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Recall from long-term memory. mode \"search\" (default): hybrid keyword+vector lookup. mode \"think\": synthesize facts with citations and knowledge gaps. mode \"graph\": traverse the knowledge graph from an entity (query = entity name; use edge/hops to steer).")]
    async fn memory_recall(
        &self,
        Parameters(p): Parameters<RecallParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let query = p.query;
        let edge = p.edge;
        let hops = p.hops.unwrap_or(2);

        match p.mode.as_deref().unwrap_or("search") {
            "graph" => {
                run_block(store, emb, move |s, _e| {
                    let doc = s
                        .resolve_doc(&query)?
                        .ok_or_else(|| EvoError::DocNotFound(query.clone()))?;
                    let edges = search::graph::traverse(s, doc.id, edge.as_deref(), hops)?;
                    Ok(serde_json::json!({ "start": doc.slug, "edges": edges }))
                })
                .await
                .map(Json)
            }
            "think" => {
                run_block(store, emb, move |s, e| {
                    let resp = think::think(s, e, &query, parse_mode(Some("balanced")), chrono::Utc::now())?;
                    Ok(serde_json::to_value(&resp).expect("serializable"))
                })
                .await
                .map(Json)
            }
            _ => {
                run_block(store, emb, move |s, e| {
                    let resp = search::search(s, e, &query, parse_mode(None), 0.03)?;
                    Ok(serde_json::to_value(&resp).expect("serializable"))
                })
                .await
                .map(Json)
            }
        }
    }

    #[tool(description = "Forget (soft-delete) a captured document by slug/title/alias. Removes its markdown file and re-indexes so it no longer surfaces in recall.")]
    async fn memory_forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let slug = p.slug;
        run_block(store, emb, move |s, e| {
            let doc = s
                .resolve_doc(&slug)?
                .ok_or_else(|| EvoError::DocNotFound(slug.clone()))?;
            let path = s.brain_root.join(format!("{}.md", doc.slug));
            std::fs::remove_file(&path)?;
            // Re-sync: soft-deletes the missing doc and resolves dangling links.
            ingest::sync_dir(s, e)?;
            Ok(serde_json::json!({ "slug": doc.slug, "forgotten": true }))
        })
        .await
        .map(Json)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Entry point
// ────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root: PathBuf =
        std::env::var("EVOMEM_ROOT").unwrap_or_else(|_| "./vault".to_string()).into();
    std::fs::create_dir_all(&root)?;
    let root_display = root.display().to_string();
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let state = AppState {
        root,
        stores: Arc::new(Mutex::new(HashMap::new())),
        embedder: Arc::new(HashEmbedder),
    };

    let server = EvomemServer { state };

    let mut config = StreamableHttpServerConfig::default().with_json_response(true);
    // rmcp only accepts loopback Host headers by default (DNS-rebinding guard).
    // Let the operator widen it: EVOMEM_ALLOWED_HOSTS=host1,host2,... or "*"
    // (allow any Host). Leave unset for the secure loopback-only default.
    if let Ok(hosts) = std::env::var("EVOMEM_ALLOWED_HOSTS") {
        let list: Vec<String> = hosts
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if list.iter().any(|h| h == "*") {
            config = config.disable_allowed_hosts();
        } else if !list.is_empty() {
            config = config.with_allowed_hosts(list);
        }
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        config,
    );

    let app = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("evomem-mcp-rs listening on http://{bind}/mcp (root: {root_display})");
    axum::serve(listener, app).await?;
    Ok(())
}
