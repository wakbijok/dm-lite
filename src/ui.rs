//! Optional local graph viewer (`dmem ui`, feature `ui`). Serves ONE embedded, offline page that
//! renders the memory graph: records and entities as nodes, edges as links. Read-only, no auth,
//! reads the CURRENT tenant via `Memory::open()` (local store or the loopback daemon, same as the
//! CLI). Manual start/stop (Ctrl-C). The page (HTML + CSS + a dependency-free canvas force graph)
//! is compiled into the binary with `include_str!`, so this stays a single binary with no CDN or
//! runtime assets. It is a single-user local viewer, not a multi-tenant server.

use crate::entry::{Edge, Entry};
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
    // Memory access is blocking (SQLite, or HTTP to the daemon); keep it off the async worker.
    // Never 500 the viewer: on any error return an empty graph plus the message for the page.
    let v = tokio::task::spawn_blocking(build_graph)
        .await
        .map_err(|e| anyhow::anyhow!("graph task: {e}"))
        .and_then(|r| r)
        .unwrap_or_else(|e| json!({ "error": e.to_string(), "nodes": [], "edges": [] }));
    Json(v)
}

/// Build the graph payload for the current tenant: current records (capped) + the edge layer.
fn build_graph() -> Result<Value> {
    let m = Memory::open()?;
    let records = m.recent(5000)?;
    let edges = m.all_edges(20_000)?;
    Ok(graph_json(&records, &edges))
}

/// Pure shaping of the payload (records -> nodes, edges -> links). Node id is the uri; `label` is
/// the title carried as a JSON string (data), so the page escapes it before any DOM use - a title
/// is never markup here.
fn graph_json(records: &[Entry], edges: &[Edge]) -> Value {
    let nodes: Vec<Value> = records
        .iter()
        .map(|e| json!({ "id": e.uri, "label": e.title, "kind": e.kind.as_str() }))
        .collect();
    let links: Vec<Value> = edges
        .iter()
        .map(|e| json!({ "from": e.from_uri, "to": e.to_uri, "rel": e.rel }))
        .collect();
    json!({ "nodes": nodes, "edges": links })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;

    #[test]
    fn graph_json_shape_and_title_is_data_not_markup() {
        let e = Entry::new_now(
            "daimon://x".into(),
            Kind::Org,
            "ns".into(),
            "<script>alert(1)</script>".into(),
            "b".into(),
            vec![],
            50,
            "daimon://x".into(),
        );
        let edges = vec![Edge { from_uri: "daimon://x".into(), to_uri: "daimon://y".into(), rel: "for".into() }];
        let g = graph_json(&[e], &edges);
        assert_eq!(g["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(g["nodes"][0]["kind"], "org");
        // the title is a JSON string value (data); the page esc()'s it before any innerHTML use
        assert_eq!(g["nodes"][0]["label"], "<script>alert(1)</script>");
        assert_eq!(g["edges"][0]["rel"], "for");
    }
}
