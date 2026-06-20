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

use crate::tools::{LocalMemory, Memory};
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

/// Resolve the bearer token to an identity: the IAM token DB first, then the env fallback
/// (which always yields a member). None = unknown/revoked/suspended.
fn resolve_identity(st: &AppState, headers: &HeaderMap) -> Option<crate::iam::Identity> {
    let h = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    let token = parse_bearer(h)?;
    if let Ok(iam) = crate::iam::Iam::open() {
        if let Some(id) = iam.resolve(token) {
            return Some(id);
        }
    }
    st.auth
        .tenant_for(Some(h))
        .map(|t| crate::iam::Identity { tenant: Some(t), is_admin: false })
}

/// Resolve the request's member tenant (admin tokens have no tenant -> None -> 401 here).
fn tenant_of(st: &AppState, headers: &HeaderMap) -> Option<String> {
    resolve_identity(st, headers).and_then(|id| id.tenant)
}

/// Run `f` only for a valid ADMIN token (403 for a member, 401 for none).
fn with_admin(st: &AppState, headers: &HeaderMap, f: impl FnOnce() -> Result<serde_json::Value>) -> ApiResp {
    match resolve_identity(st, headers) {
        Some(id) if id.is_admin => match f() {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => internal(e),
        },
        Some(_) => err(StatusCode::FORBIDDEN, "admin token required"),
        None => err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    }
}

#[derive(Deserialize)]
struct RecallReq {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    /// bitemporal: recall the store as of this epoch-ms (and facts valid then)
    #[serde(default)]
    as_of: Option<i64>,
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
struct LessonReq {
    title: String,
    lesson: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct IncidentReq {
    title: String,
    impact: String,
    #[serde(default)]
    resolution: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct RunbookReq {
    title: String,
    steps: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ConventionReq {
    title: String,
    rule: String,
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

#[derive(Deserialize)]
struct RecentReq {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct HistoryReq {
    uri: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ForgetReq {
    uri: String,
}

fn ns_or<'a>(ns: &'a Option<String>, default: &'a str) -> &'a str {
    ns.as_deref().filter(|s| !s.is_empty()).unwrap_or(default)
}

/// Auth, open the request's tenant store, run `f`, and JSON-encode its result. `client_err`
/// maps a failure to 400 (treat as bad input) instead of 500.
fn with_tenant(
    st: &AppState,
    headers: &HeaderMap,
    client_err: bool,
    f: impl FnOnce(&LocalMemory) -> Result<serde_json::Value>,
) -> ApiResp {
    let tenant = match tenant_of(st, headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    match Memory::open_tenant(&tenant).and_then(|m| f(&m)) {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => {
            if client_err {
                bad_request(e)
            } else {
                internal(e)
            }
        }
    }
}

async fn healthz() -> ApiResp {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn recall_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecallReq>) -> ApiResp {
    with_tenant(&st, &headers, false, |m| {
        let limit = req.limit.unwrap_or(6);
        let hits = match req.as_of {
            Some(ts) => m.recall_as_of(&req.query, limit, ts, ts)?,
            None => m.recall(&req.query, limit)?,
        };
        Ok(json!(hits))
    })
}

async fn recent_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecentReq>) -> ApiResp {
    with_tenant(&st, &headers, false, |m| Ok(json!(m.recent(req.limit.unwrap_or(10))?)))
}

async fn persona_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    with_tenant(&st, &headers, false, |m| Ok(json!(m.persona()?)))
}

async fn history_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<HistoryReq>) -> ApiResp {
    with_tenant(&st, &headers, false, |m| Ok(json!(m.history(&req.uri, req.limit.unwrap_or(20))?)))
}

async fn forget_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ForgetReq>) -> ApiResp {
    with_tenant(&st, &headers, false, |m| Ok(json!({ "forgotten": m.forget(&req.uri)? })))
}

async fn remember_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RememberReq>) -> ApiResp {
    with_tenant(&st, &headers, false, |m| {
        Ok(json!({ "uri": m.remember(&req.text, ns_or(&req.namespace, "resources/notes"))? }))
    })
}

async fn decision_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<DecisionReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        let ns = ns_or(&req.namespace, "resources/notes");
        Ok(json!({ "uri": m.log_decision(&req.title, &req.context, &req.decision, &req.rationale, ns)? }))
    })
}

async fn lesson_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<LessonReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        Ok(json!({ "uri": m.log_lesson(&req.title, &req.lesson, ns_or(&req.namespace, "agent/lessons"))? }))
    })
}

async fn incident_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<IncidentReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        let ns = ns_or(&req.namespace, "resources/incidents");
        Ok(json!({ "uri": m.log_incident(&req.title, &req.impact, &req.resolution, ns)? }))
    })
}

async fn runbook_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RunbookReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        Ok(json!({ "uri": m.log_runbook(&req.title, &req.steps, ns_or(&req.namespace, "resources/runbooks"))? }))
    })
}

async fn convention_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ConventionReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        Ok(json!({ "uri": m.log_convention(&req.title, &req.rule, ns_or(&req.namespace, "resources/conventions"))? }))
    })
}

async fn reminder_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ReminderReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        Ok(json!({ "uri": m.add_reminder(&req.title, &req.text, ns_or(&req.namespace, "agent/reminders"))? }))
    })
}

#[derive(Deserialize)]
struct ImportReq {
    kind: String,
    #[serde(default)]
    namespace: String,
    title: String,
    #[serde(default)]
    body: String,
    /// original creation time (migration); 0/absent = now
    #[serde(default)]
    created_ms: i64,
}

async fn import_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ImportReq>) -> ApiResp {
    with_tenant(&st, &headers, true, |m| {
        let kind = crate::entry::Kind::from_str(&req.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", req.kind))?;
        let ns = if req.namespace.is_empty() { "resources/notes" } else { &req.namespace };
        let uri = if req.created_ms > 0 {
            m.import_record_at(kind, ns, &req.title, &req.body, req.created_ms)?
        } else {
            m.import_record(kind, ns, &req.title, &req.body)?
        };
        Ok(json!({ "uri": uri }))
    })
}

// --- admin (IAM) routes: require the root admin token ---

#[derive(Deserialize)]
struct AdminAddReq {
    tenant: String,
    #[serde(default)]
    display: String,
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
struct AdminTargetReq {
    target: String,
}

async fn admin_add_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminAddReq>) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        let (tenant, token) = iam.create_tenant(&req.tenant, &req.display, &req.label)?;
        Ok(json!({ "tenant": tenant, "token": token }))
    })
}

async fn admin_list_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        let rows: Vec<_> = iam
            .list()?
            .into_iter()
            .map(|(t, s, n)| json!({ "tenant": t, "status": s, "tokens": n }))
            .collect();
        Ok(json!(rows))
    })
}

async fn admin_revoke_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminTargetReq>) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        Ok(json!({ "revoked": iam.revoke(&req.target)? }))
    })
}

async fn admin_rm_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminTargetReq>) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        iam.remove_tenant(&req.target)?;
        Ok(json!({ "removed": req.target }))
    })
}

/// Assemble the router. `/healthz` is open; every other route requires a valid bearer token.
pub fn router(auth: Arc<dyn Authenticator>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/recall", post(recall_h))
        .route("/recent", post(recent_h))
        .route("/persona", post(persona_h))
        .route("/history", post(history_h))
        .route("/forget", post(forget_h))
        .route("/remember", post(remember_h))
        .route("/log_decision", post(decision_h))
        .route("/log_lesson", post(lesson_h))
        .route("/log_incident", post(incident_h))
        .route("/log_runbook", post(runbook_h))
        .route("/log_convention", post(convention_h))
        .route("/add_reminder", post(reminder_h))
        .route("/import", post(import_h))
        .route("/admin/tenant", post(admin_add_h))
        .route("/admin/tenants", get(admin_list_h))
        .route("/admin/revoke", post(admin_revoke_h))
        .route("/admin/rm", post(admin_rm_h))
        .with_state(AppState { auth })
}

/// TLS choice for the server: bring-your-own cert/key, or generate a self-signed pair.
pub struct TlsOpts {
    pub cert: Option<String>,
    pub key: Option<String>,
    pub generate: bool,
}

/// Generate a self-signed cert + key (PEM), persisting them under `<data>/tls/` so clients
/// can trust the cert via `ca_cert`. SANs cover localhost and the bind host.
fn generate_self_signed(addr: &str) -> Result<(String, String)> {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    let mut sans = vec!["localhost".to_string()];
    if !host.is_empty() && host != "0.0.0.0" && host != "localhost" {
        sans.push(host.to_string());
    }
    let ck = rcgen::generate_simple_self_signed(sans).map_err(|e| anyhow::anyhow!("rcgen: {e}"))?;
    let cert_pem = ck.cert.pem();
    let key_pem = ck.key_pair.serialize_pem();
    if let Ok(dir) = crate::config::data_dir() {
        let tdir = dir.join("tls");
        let _ = std::fs::create_dir_all(&tdir);
        let cpath = tdir.join("cert.pem");
        let _ = std::fs::write(&cpath, &cert_pem);
        let _ = std::fs::write(tdir.join("key.pem"), &key_pem);
        eprintln!("dmem serve: generated self-signed cert at {}", cpath.display());
        eprintln!("           clients: set `ca_cert` to that file (or `insecure = true`)");
    }
    Ok((cert_pem, key_pem))
}

/// Bind `addr` and serve. With TLS (cert/key or generate) it serves HTTPS; otherwise plain
/// HTTP with a loud warning. Tokens come from the environment.
pub fn run_blocking(addr: &str, tls: TlsOpts) -> Result<()> {
    // rustls 0.23 needs a process-wide crypto provider installed before any TLS work.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let auth = BearerAuth::from_env()?;
    // IAM: ensure a bootstrap root admin token; print it once if newly generated.
    if let Ok(iam) = crate::iam::Iam::open() {
        match iam.ensure_admin() {
            Ok(Some(token)) => {
                let dir = crate::config::data_dir().map(|d| d.display().to_string()).unwrap_or_default();
                eprintln!("dmem serve: generated ROOT ADMIN token (save it, shown once):");
                eprintln!("    {token}");
                eprintln!("  also written to {dir}/admin.token (0600)");
                eprintln!("  wire the admin client: dmem login {addr} {token}  then `dmem admin add <tenant>`");
            }
            Ok(None) => {}
            Err(e) => eprintln!("dmem serve: IAM init warning ({e:#})"),
        }
    }
    if auth.is_empty() {
        eprintln!(
            "dmem serve: tip - create tenants with the admin token (`dmem admin add <tenant>`), \
             or set DM_TOKEN_<tenant>=<secret> for a quick static token."
        );
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = router(Arc::new(auth));
        let sock: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("bad addr {addr}: {e}"))?;

        let tls_config = if let (Some(c), Some(k)) = (&tls.cert, &tls.key) {
            Some(
                axum_server::tls_rustls::RustlsConfig::from_pem_file(c, k)
                    .await
                    .map_err(|e| anyhow::anyhow!("load TLS cert/key: {e}"))?,
            )
        } else if tls.generate {
            let (cert_pem, key_pem) = generate_self_signed(addr)?;
            Some(
                axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
                    .await
                    .map_err(|e| anyhow::anyhow!("self-signed TLS: {e}"))?,
            )
        } else {
            None
        };

        match tls_config {
            Some(cfg) => {
                eprintln!("dmem serve: listening on https://{addr}");
                axum_server::bind_rustls(sock, cfg)
                    .serve(app.into_make_service())
                    .await
                    .map_err(|e| anyhow::anyhow!("serve (tls): {e}"))?;
            }
            None => {
                eprintln!("dmem serve: WARNING serving plain HTTP on http://{addr} (no TLS).");
                eprintln!("           use --tls-cert/--tls-key or --tls-generate for HTTPS.");
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = tokio::signal::ctrl_c().await;
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
            }
        }
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
