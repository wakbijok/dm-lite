//! Optional local graph viewer (`dmem ui`, feature `ui`). Serves ONE embedded, offline page that
//! renders the memory graph: records and entities as nodes, edges as links. Read-only, no auth,
//! reads the CURRENT tenant via `Memory::open()` (local store or the loopback daemon, same as the
//! CLI). Manual start/stop (Ctrl-C). The page (HTML + CSS + a dependency-free canvas force graph)
//! is compiled into the binary with `include_str!`, so this stays a single binary with no CDN or
//! runtime assets. It is a single-user local viewer, not a multi-tenant server.

use crate::tools::Memory;
use anyhow::Result;
use axum::{response::Html, routing::get, Json, Router};
use serde_json::{json, Value};

/// The whole viewer UI, compiled in.
const PAGE: &str = include_str!("ui_graph.html");

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

async fn graph() -> Json<Value> {
    // Never 500 the viewer: on error, return an empty graph plus the message for the page to show.
    Json(build_graph().unwrap_or_else(|e| json!({ "error": e.to_string(), "nodes": [], "edges": [] })))
}

/// Build {nodes, edges} for the current tenant: nodes are current records (capped), edges are the
/// graph layer. Node id is the uri; kind drives the color in the page.
fn build_graph() -> Result<Value> {
    let m = Memory::open()?;
    let records = m.recent(5000)?;
    let nodes: Vec<Value> = records
        .iter()
        .map(|e| json!({ "id": e.uri, "label": e.title, "kind": e.kind.as_str() }))
        .collect();
    let edges: Vec<Value> = m
        .all_edges(20_000)?
        .into_iter()
        .map(|e| json!({ "from": e.from_uri, "to": e.to_uri, "rel": e.rel }))
        .collect();
    Ok(json!({ "nodes": nodes, "edges": edges }))
}

/// Serve the viewer on `addr` until Ctrl-C. With `open`, also launch the default browser.
pub fn run(addr: &str, open: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = Router::new().route("/", get(index)).route("/graph", get(graph));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        eprintln!("dmem ui: graph viewer on http://{addr}  (Ctrl-C to stop)");
        if open {
            let _ = std::process::Command::new(open_cmd()).arg(format!("http://{addr}")).spawn();
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(target_os = "linux")]
fn open_cmd() -> &'static str {
    "xdg-open"
}
#[cfg(not(target_os = "linux"))]
fn open_cmd() -> &'static str {
    "open"
}
