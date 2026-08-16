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
use evomem::api::CaptureRequest;
use evomem::embed::{Embedder, HashEmbedder};
use evomem::error::EvoError;
use evomem::model::Mode;
use evomem::store::Store;
use evomem::{capture, ingest, search, stats, think};
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
struct SearchParams {
    /// The query to search for.
    query: String,
    /// Retrieval mode: conservative | balanced | tokenmax.
    mode: Option<String>,
    /// Maximum number of hits.
    limit: Option<usize>,
    /// Minimum relevance score (0.0 – 1.0).
    min_score: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ThinkParams {
    /// Open question to reason over.
    query: String,
    /// Retrieval mode: conservative | balanced | tokenmax.
    mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GraphParams {
    /// Start entity: slug, title, or alias.
    start: String,
    /// Only follow edges of this type (e.g. works_at, founded).
    edge: Option<String>,
    /// How many hops to traverse.
    hops: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CaptureParams {
    /// Fact/thought to capture.
    text: String,
    /// Optional title (derived from text if omitted).
    title: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DocParams {
    /// Document slug (e.g. "people/alice" or "alice").
    slug: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Tools
// ────────────────────────────────────────────────────────────────────────────

#[tool_router(server_handler)]
impl EvomemServer {
    #[tool(description = "Ensure the client's namespace brain exists (creates the directory + database). Idempotent.")]
    async fn memory_init(&self, ctx: RequestContext<RoleServer>) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        self.resolve_store(&ns)?;
        let brain = self.state.root.join(&ns);
        Ok(Json(serde_json::json!({
            "namespace": ns,
            "brain": brain.display().to_string(),
            "ready": true,
        })))
    }

    #[tool(description = "Re-index all markdown files in the client's namespace (disk is the source of truth).")]
    async fn memory_sync(&self, ctx: RequestContext<RoleServer>) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let report = run_block(store, emb, |s, e| ingest::sync_dir(s, e)).await?;
        Ok(Json(serde_json::to_value(&report).map_err(|e| e.to_string())?))
    }

    #[tool(description = "Hybrid retrieval (lexical + vector + knowledge graph), deterministic, no LLM at query time.")]
    async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let mode = parse_mode(p.mode.as_deref());
        let min_score = p.min_score.unwrap_or(0.03);
        let limit = p.limit;
        let query = p.query;
        run_block(store, emb, move |s, e| {
            let mut resp = search::search(s, e, &query, mode, min_score)?;
            if let Some(l) = limit {
                resp.hits.truncate(l);
            }
            Ok(serde_json::to_value(&resp).expect("serializable"))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Knowledge synthesis with citations + gap analysis (what is known and what is missing).")]
    async fn memory_think(
        &self,
        Parameters(p): Parameters<ThinkParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let mode = parse_mode(p.mode.as_deref());
        let query = p.query;
        run_block(store, emb, move |s, e| {
            let resp = think::think(s, e, &query, mode, chrono::Utc::now())?;
            Ok(serde_json::to_value(&resp).expect("serializable"))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Traverse the typed knowledge graph from an entity (multi-hop).")]
    async fn memory_graph(
        &self,
        Parameters(p): Parameters<GraphParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let hops = p.hops.unwrap_or(2);
        let edge = p.edge.clone();
        let start = p.start;
        run_block(store, emb, move |s, _e| {
            let doc = s
                .resolve_doc(&start)?
                .ok_or_else(|| EvoError::DocNotFound(start.clone()))?;
            let edges = search::graph::traverse(s, doc.id, edge.as_deref(), hops)?;
            Ok(serde_json::json!({ "start": doc.slug, "edges": edges }))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Capture a quick fact/thought into inbox/ and index it immediately.")]
    async fn memory_capture(
        &self,
        Parameters(p): Parameters<CaptureParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let text = p.text;
        let title = p.title;
        run_block(store, emb, move |s, e| {
            let resp =
                capture::capture(s, e, &CaptureRequest { text, title }, chrono::Utc::now())?;
            Ok(serde_json::to_value(&resp).expect("serializable"))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Read the full content of a single document by slug.")]
    async fn memory_get_doc(
        &self,
        Parameters(p): Parameters<DocParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let slug = p.slug;
        run_block(store, emb, move |s, _e| {
            let doc = s
                .resolve_doc(&slug)?
                .ok_or_else(|| EvoError::DocNotFound(slug.clone()))?;
            let content =
                std::fs::read_to_string(s.brain_root.join(format!("{}.md", doc.slug)))
                    .unwrap_or_default();
            Ok(serde_json::json!({
                "slug": doc.slug,
                "title": doc.title,
                "type": doc.doc_type,
                "tags": doc.tags,
                "updated_at": doc.updated_at,
                "content": content,
            }))
        })
        .await
        .map(Json)
    }

    #[tool(description = "Knowledge store statistics for the client's namespace.")]
    async fn memory_stats(&self, ctx: RequestContext<RoleServer>) -> Result<Json<Value>, String> {
        let ns = self.resolve_namespace(&ctx)?;
        let store = self.resolve_store(&ns)?;
        let emb = Arc::clone(&self.state.embedder);
        let report = run_block(store, emb, |s, _e| stats::stats(s)).await?;
        Ok(Json(serde_json::to_value(&report).map_err(|e| e.to_string())?))
    }

    #[tool(description = "List all namespace brains (directories containing a .evomem.db).")]
    fn memory_list_namespaces(&self) -> Result<Json<Value>, String> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.state.root) {
            for entry in entries.flatten() {
                if entry.path().join(".evomem.db").is_file() {
                    if let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(Json(serde_json::json!({ "namespaces": names })))
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
