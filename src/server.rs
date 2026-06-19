//! Server mode (feature `server`): a small axum + tokio HTTP API over the database-per-
//! tenant store. Auth is multi-token bearer -> tenant (matching v1 daimon-memory's
//! DAIMON_API_KEY model): each `DM_TOKEN_<TENANT>=secret` env var registers a token that
//! resolves to that tenant. The tenant is resolved PER REQUEST (never via the process-global
//! $DM_TENANT, which would race), and `Memory::open_tenant` opens that tenant's store.
//!
//! Routes mirror the MCP/CLI tool surface. SQLite work runs synchronously inside the async
//! handler (no await held across it); at this scale (tens-to-~100 users over per-tenant
//! SQLite, whose writes serialize anyway) that is correct and simple. A per-tenant Memory
//! cache is a deliberate follow-on, not needed for correctness.

use crate::tools::Memory;
use anyhow::Result;
use axum::{
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

type ApiResp = (StatusCode, Json<Value>);

/// Resolve an `Authorization` header to a tenant. The seam: BearerAuth now, JWT could drop
/// in later without touching handlers.
pub trait Authenticator: Send + Sync {
    fn tenant_for(&self, auth_header: Option<&str>) -> Option<String>;
}

/// Multi-token bearer auth: a token -> tenant map built from `DM_TOKEN_<TENANT>` env vars.
pub struct BearerAuth {
    map: HashMap<String, String>,
}

impl BearerAuth {
    /// Build the token -> tenant map from `DM_TOKEN_<TENANT>` env vars. Fails fast on an
    /// ambiguous config: the same secret mapping to two different tenants would otherwise
    /// resolve nondeterministically (HashMap iteration order), silently breaking isolation.
    pub fn from_env() -> Result<Self> {
        let mut map: HashMap<String, String> = HashMap::new();
        for (k, v) in std::env::vars() {
            if let Some(tenant) = k.strip_prefix("DM_TOKEN_") {
                if tenant.is_empty() || v.is_empty() {
                    continue;
                }
                let tenant = crate::config::canonical_tenant(tenant);
                if let Some(prev) = map.insert(v, tenant.clone()) {
                    if prev != tenant {
                        anyhow::bail!(
                            "ambiguous DM_TOKEN config: one bearer secret maps to both tenants '{prev}' and '{tenant}'"
                        );
                    }
                }
            }
        }
        Ok(BearerAuth { map })
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Extract the credential from a `Bearer <token>` header (scheme is case-insensitive).
fn parse_bearer(h: &str) -> Option<&str> {
    let (scheme, rest) = h.trim().split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(rest.trim())
    } else {
        None
    }
}

impl Authenticator for BearerAuth {
    fn tenant_for(&self, auth_header: Option<&str>) -> Option<String> {
        let token = parse_bearer(auth_header?)?;
        self.map.get(token).cloned()
    }
}

#[derive(Clone)]
struct AppState {
    auth: Arc<dyn Authenticator>,
}

fn err(code: StatusCode, msg: &str) -> ApiResp {
    (code, Json(json!({ "error": msg })))
}

/// Log the full error chain server-side; return a generic body. Never leak internals (the
/// anyhow chain includes absolute DB paths) to clients, even authenticated ones.
fn internal(e: anyhow::Error) -> ApiResp {
    eprintln!("dmem serve: handler error: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// As `internal`, but for the typed-save routes where the failure is usually client input.
fn bad_request(e: anyhow::Error) -> ApiResp {
    eprintln!("dmem serve: handler error: {e:#}");
    err(StatusCode::BAD_REQUEST, "invalid request")
}

/// Resolve the request's tenant from its Authorization header, or None (-> 401).
fn tenant_of(st: &AppState, headers: &HeaderMap) -> Option<String> {
    let h = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    st.auth.tenant_for(h)
}

#[derive(Deserialize)]
struct RecallReq {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct RememberReq {
    text: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct DecisionReq {
    title: String,
    #[serde(default)]
    context: String,
    decision: String,
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ReminderReq {
    title: String,
    text: String,
    #[serde(default)]
    namespace: Option<String>,
}

fn ns_or<'a>(ns: &'a Option<String>, default: &'a str) -> &'a str {
    ns.as_deref().filter(|s| !s.is_empty()).unwrap_or(default)
}

async fn healthz() -> ApiResp {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn recall_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecallReq>) -> ApiResp {
    let tenant = match tenant_of(&st, &headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    let limit = req.limit.unwrap_or(6);
    match Memory::open_tenant(&tenant).and_then(|m| m.recall(&req.query, limit)) {
        Ok(hits) => (StatusCode::OK, Json(json!(hits))),
        Err(e) => internal(e),
    }
}

async fn remember_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RememberReq>) -> ApiResp {
    let tenant = match tenant_of(&st, &headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    let ns = ns_or(&req.namespace, "resources/notes").to_string();
    match Memory::open_tenant(&tenant).and_then(|m| m.remember(&req.text, &ns)) {
        Ok(uri) => (StatusCode::OK, Json(json!({ "uri": uri }))),
        Err(e) => internal(e),
    }
}

async fn decision_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<DecisionReq>) -> ApiResp {
    let tenant = match tenant_of(&st, &headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    let ns = ns_or(&req.namespace, "resources/notes").to_string();
    match Memory::open_tenant(&tenant)
        .and_then(|m| m.log_decision(&req.title, &req.context, &req.decision, &req.rationale, &ns))
    {
        Ok(uri) => (StatusCode::OK, Json(json!({ "uri": uri }))),
        Err(e) => bad_request(e),
    }
}

async fn reminder_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ReminderReq>) -> ApiResp {
    let tenant = match tenant_of(&st, &headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    let ns = ns_or(&req.namespace, "agent/reminders").to_string();
    match Memory::open_tenant(&tenant).and_then(|m| m.add_reminder(&req.title, &req.text, &ns)) {
        Ok(uri) => (StatusCode::OK, Json(json!({ "uri": uri }))),
        Err(e) => bad_request(e),
    }
}

/// Assemble the router. `/healthz` is open; every other route requires a valid bearer token.
pub fn router(auth: Arc<dyn Authenticator>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/recall", post(recall_h))
        .route("/remember", post(remember_h))
        .route("/log_decision", post(decision_h))
        .route("/add_reminder", post(reminder_h))
        .with_state(AppState { auth })
}

/// Bind `addr` and serve until Ctrl-C (graceful shutdown). Tokens come from the environment.
pub fn run_blocking(addr: &str) -> Result<()> {
    let auth = BearerAuth::from_env()?;
    if auth.is_empty() {
        eprintln!(
            "dmem serve: no DM_TOKEN_<tenant> tokens in the environment; authed routes will 401 \
             (only /healthz is open). Set e.g. DM_TOKEN_ACME=<secret> to grant tenant 'acme'."
        );
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = router(Arc::new(auth));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
        eprintln!("dmem serve: listening on http://{addr}");
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    // These tests mutate process-global env (DM_DATA_DIR, DM_TOKEN_*). Cargo runs tests in a
    // binary multithreaded, so they must serialize on this lock; any future env-reading test
    // in this binary must take it too.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn bearer_resolves_tenant() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DM_TOKEN_ACME", "secret123");
        let a = BearerAuth::from_env().unwrap();
        assert_eq!(a.tenant_for(Some("Bearer secret123")).as_deref(), Some("acme"));
        assert_eq!(a.tenant_for(Some("bearer secret123")).as_deref(), Some("acme")); // case-insensitive
        assert_eq!(a.tenant_for(Some("Bearer nope")), None);
        assert_eq!(a.tenant_for(Some("Basic secret123")), None);
        assert_eq!(a.tenant_for(None), None);
        std::env::remove_var("DM_TOKEN_ACME");
    }

    #[test]
    fn duplicate_secret_to_different_tenants_fails_fast() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("DM_TOKEN_ACME", "shared");
        std::env::set_var("DM_TOKEN_GLOBEX", "shared");
        let r = BearerAuth::from_env();
        assert!(r.is_err(), "same secret -> two tenants must be rejected");
        std::env::remove_var("DM_TOKEN_ACME");
        std::env::remove_var("DM_TOKEN_GLOBEX");
    }

    #[tokio::test]
    async fn recall_route_authorizes_and_returns_hits() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("dmsrv-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_T1SRV", "tok1");
        // seed a record into tenant t1srv
        let m = Memory::open_tenant("t1srv").unwrap();
        m.remember("the vector substrate is zvec", "resources/notes").unwrap();

        let app = router(Arc::new(BearerAuth::from_env().unwrap()));

        // missing token -> 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/recall")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"vector","limit":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // valid token -> 200 + the seeded record
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/recall")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer tok1")
                    .body(Body::from(r#"{"query":"vector substrate","limit":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("zvec"), "expected the seeded record in body: {s}");

        std::env::remove_var("DM_TOKEN_T1SRV");
        std::env::remove_var("DM_DATA_DIR");
    }
}
