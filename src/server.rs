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
use std::sync::{Arc, Mutex};

/// Hex SHA-256, used to key the env-token map by digest rather than by the raw secret. Looking a
/// token up by its (fixed-length, high-entropy) hash avoids leaking secret bytes through the
/// timing of a raw-string comparison.
fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(s.as_bytes()) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

type ApiResp = (StatusCode, Json<Value>);

/// Upper bound on any client-supplied `limit`, so an untrusted value cannot balloon a query
/// (the deeper rescoring pool is `limit*2`) or wrap on the `LIMIT ?` cast.
const MAX_LIMIT: usize = 1000;

/// Cap on a request body: these payloads are small (a query, a memory record); a few hundred KB
/// is generous and stops an unbounded-body memory blowup.
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Resolve an `Authorization` header to a caller identity (tenant + optional agent). The seam:
/// BearerAuth now, JWT could drop in later without touching handlers.
pub trait Authenticator: Send + Sync {
    fn identity_for(&self, auth_header: Option<&str>) -> Option<crate::iam::Identity>;
}

/// Multi-token bearer auth: a token -> (tenant, agent) map built from `DM_TOKEN_<TENANT>` and
/// `DM_TOKEN_<TENANT>__<AGENT>` env vars. Keyed by the SHA-256 of the secret (not the raw
/// secret) so a lookup compares fixed-length digests.
pub struct BearerAuth {
    map: HashMap<String, (String, Option<String>)>,
}

/// Render a parsed (tenant, agent) identity back in the env-var spelling, for error messages.
fn ident_str(i: &(String, Option<String>)) -> String {
    match &i.1 {
        Some(a) => format!("{}__{}", i.0, a),
        None => i.0.clone(),
    }
}

impl BearerAuth {
    /// Build the token-hash -> identity map from the env. `DM_TOKEN_<TENANT>=secret` is an
    /// agent-less token (exactly as before); `DM_TOKEN_<TENANT>__<AGENT>=secret` also carries a
    /// per-agent identity. The FIRST double underscore splits tenant from agent (single
    /// underscores stay part of the tenant name; a trailing `__` means agent-less). Fails fast
    /// on an ambiguous config: the same secret mapping to two different identities would
    /// otherwise resolve nondeterministically (HashMap iteration order), silently breaking
    /// tenant isolation or agent attribution.
    pub fn from_env() -> Result<Self> {
        let mut map: HashMap<String, (String, Option<String>)> = HashMap::new();
        for (k, v) in std::env::vars() {
            if let Some(rest) = k.strip_prefix("DM_TOKEN_") {
                let (tenant_raw, agent_raw) = match rest.split_once("__") {
                    Some((t, a)) => (t, Some(a)),
                    None => (rest, None),
                };
                if tenant_raw.is_empty() || v.is_empty() {
                    continue;
                }
                // Weak env secrets fail FAST (audit Medium #9): env tokens skip IAM's 160-bit
                // minting, so enforce a floor here rather than silently serving a guessable
                // bearer. Same fail-fast posture as the duplicate-secret check below.
                if v.len() < 16 {
                    anyhow::bail!(
                        "DM_TOKEN_{rest} is too short ({} chars): bearer secrets must be at least 16 characters",
                        v.len()
                    );
                }
                let ident = (
                    crate::config::canonical_tenant(tenant_raw),
                    agent_raw.and_then(crate::config::canonical_agent),
                );
                if let Some(prev) = map.insert(sha256_hex(&v), ident.clone()) {
                    if prev != ident {
                        anyhow::bail!(
                            "ambiguous DM_TOKEN config: one bearer secret maps to both '{}' and '{}'",
                            ident_str(&prev),
                            ident_str(&ident)
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
    fn identity_for(&self, auth_header: Option<&str>) -> Option<crate::iam::Identity> {
        let token = parse_bearer(auth_header?)?;
        // Env tokens are always scope-unbound (full tenant): scoped identities exist only in
        // iam.db, where they can be audited and revoked.
        self.map.get(&sha256_hex(token)).map(|(tenant, agent)| crate::iam::Identity {
            tenant: Some(tenant.clone()),
            is_admin: false,
            agent: agent.clone(),
            scope_read: None,
            scope_write: None,
            adapter: false,
        })
    }
}

/// Shared, per-tenant LocalMemory handle. rusqlite Connection is Send but !Sync, so each tenant's
/// engine is behind its own Mutex; the IAM connection (also !Sync) sits behind one Mutex.
type TenantHandle = Arc<Mutex<LocalMemory>>;

#[derive(Clone)]
struct AppState {
    auth: Arc<dyn Authenticator>,
    /// The IAM connection, opened ONCE at startup (None if it could not be opened). Token
    /// resolution locks it briefly; no per-request open.
    iam: Arc<Mutex<Option<crate::iam::Iam>>>,
    /// Per-tenant engine cache: a request reuses the tenant's open SQLite/zvec handles instead of
    /// re-opening them every call. zvec's Collection is Send + Sync, so this is safe to share.
    mem: Arc<Mutex<HashMap<String, TenantHandle>>>,
}

impl AppState {
    /// The cached handle for a tenant, opening (and caching) it on first use.
    fn memory_for(&self, tenant: &str) -> Result<TenantHandle> {
        let mut cache = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(m) = cache.get(tenant) {
            return Ok(m.clone());
        }
        let handle: TenantHandle = Arc::new(Mutex::new(Memory::open_tenant(tenant)?));
        cache.insert(tenant.to_string(), handle.clone());
        Ok(handle)
    }
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

/// Resolve the bearer token to an identity: the IAM token DB first (revocation/suspension
/// enforced), then the env-token fallback. None = unknown/revoked/suspended. Uses the startup
/// IAM handle (locked briefly); if IAM was unavailable at startup the map is None and only env
/// tokens resolve, which was logged loudly then (a stale IAM no longer silently fails per request).
fn resolve_identity(st: &AppState, headers: &HeaderMap) -> Option<crate::iam::Identity> {
    let h = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    let token = parse_bearer(h)?;
    {
        let iam = st.iam.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(iam) = iam.as_ref() {
            if let Some(id) = iam.resolve(token) {
                return Some(id);
            }
        }
    }
    st.auth.identity_for(Some(h))
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
    /// bitemporal: recall the store as of this epoch-ms (system time)
    #[serde(default)]
    as_of: Option<i64>,
    /// bitemporal: facts valid at this epoch-ms; defaults to `as_of` when absent (older clients).
    #[serde(default)]
    valid: Option<i64>,
}

#[derive(Deserialize)]
struct RememberReq {
    text: String,
    #[serde(default)]
    namespace: Option<String>,
    /// bitemporal valid interval (application time); absent = now / open
    #[serde(default)]
    valid_from: Option<i64>,
    #[serde(default)]
    valid_to: Option<i64>,
}

#[derive(Deserialize)]
struct InvalidateReq {
    uri: String,
    /// epoch-ms from which the fact is no longer true
    valid_to: i64,
}

#[derive(Deserialize)]
struct LinkReq {
    from: String,
    to: String,
    rel: String,
}

#[derive(Deserialize)]
struct EdgesReq {
    uri: String,
}

#[derive(Deserialize)]
struct EdgesAllReq {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct NeighborsReq {
    seeds: Vec<String>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct RecallExpandedReq {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    depth: Option<usize>,
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

/// Auth, get the request's (cached) tenant handle, run the blocking `f` on the blocking pool, and
/// JSON-encode its result. `f` receives the token's agent identity (None for agent-less and
/// legacy tokens) alongside the tenant's memory: the tenant handle is CACHED PER TENANT and
/// shared by every agent on it (one shared memory), so the agent must ride the request, never
/// the handle. `client_err` maps a failure to 400 (bad input) instead of 500. `f` runs under the
/// tenant's Mutex via spawn_blocking, so SQLite/zvec work never blocks an async worker and
/// same-tenant requests serialize (SQLite writes serialize anyway) while different tenants run
/// in parallel. (Admin tokens have no tenant -> 401 here.)
/// Per-request scope binding derived from the token (+ adapter headers): (write, read).
/// Scope-unbound tokens (every pre-scope token) bind to full tenant - byte-identical to the
/// pre-scope server. Non-adapter tokens asserting scope headers are silently ignored (Q1).
fn scope_binding(id: &crate::iam::Identity, headers: &HeaderMap) -> (Option<String>, Option<Vec<String>>) {
    if id.scope_unbound() {
        return (Some(String::new()), None);
    }
    if id.adapter {
        // Q1: the adapter asserts the READER's scope set per request; an absent header means
        // global-only (fail closed). Token-level scope_read, when present, caps the set.
        let mut read: Vec<String> = headers
            .get("x-dm-scopes")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default();
        if let Some(cap) = &id.scope_read {
            read.retain(|s| cap.contains(s));
        }
        // Adapter writes are explicit per request too: header absent = this request is
        // read-only; an empty value = deliberate GLOBAL write (Q3's explicit promotion path).
        let write = headers.get("x-dm-write-scope").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
        if let Some(w) = write.as_deref().filter(|w| !w.is_empty()) {
            if !read.iter().any(|s| s == w) {
                read.push(w.to_string());
            }
        }
        return (write, Some(read));
    }
    // Plain scope-bound token: static grants. Missing scope_read = global-only (fail closed);
    // the write scope is always readable so engine-side mutation guards can see their target.
    let mut read = id.scope_read.clone().unwrap_or_default();
    if let Some(w) = id.scope_write.as_deref().filter(|w| !w.is_empty()) {
        if !read.iter().any(|s| s == w) {
            read.push(w.to_string());
        }
    }
    (id.scope_write.clone(), Some(read))
}

async fn with_tenant<F>(st: &AppState, headers: &HeaderMap, client_err: bool, f: F) -> ApiResp
where
    F: FnOnce(&LocalMemory, Option<String>) -> Result<serde_json::Value> + Send + 'static,
{
    let id = match resolve_identity(st, headers) {
        Some(id) if id.tenant.is_some() => id,
        _ => return err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    };
    let tenant = id.tenant.clone().expect("checked above");
    let agent = id.agent.clone();
    let (write_scope, read_scopes) = scope_binding(&id, headers);
    let handle = match st.memory_for(&tenant) {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let res = tokio::task::spawn_blocking(move || {
        let mut guard = handle.lock().unwrap_or_else(|p| p.into_inner());
        // Bind this request's scope context, run, then RESET: the handle is cached per
        // tenant and the next request must never inherit a previous caller's context.
        guard.set_scope_context(write_scope.as_deref(), read_scopes);
        guard.set_agent_view(agent.clone());
        let r = f(&guard, agent);
        guard.set_scope_context(Some(""), None);
        guard.set_agent_view(None);
        r
    })
    .await;
    match res {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => {
            if client_err {
                bad_request(e)
            } else {
                internal(e)
            }
        }
        Err(e) => internal(anyhow::anyhow!("memory task failed: {e}")),
    }
}

async fn healthz() -> ApiResp {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn recall_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecallReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| {
        let limit = req.limit.unwrap_or(6).min(MAX_LIMIT);
        let hits = match req.as_of {
            Some(ts) => m.recall_as_of(&req.query, limit, ts, req.valid.unwrap_or(ts))?,
            None => m.recall(&req.query, limit)?,
        };
        Ok(json!(hits))
    })
    .await
}

async fn recent_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecentReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!(m.recent(req.limit.unwrap_or(10).min(MAX_LIMIT))?))).await
}

async fn persona_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    // The token's agent identity picks the persona set: an agent gets shared governance + its
    // own agents/<agent>/ records; an agent-less token keeps the legacy everything.
    with_tenant(&st, &headers, false, |m, agent| Ok(json!(m.persona_for(agent.as_deref())?))).await
}

async fn reminders_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecentReq>) -> ApiResp {
    // /reminders is the SoT inventory endpoint: default to the FULL open list. A small default
    // here silently truncated the backlog (a dated hospital appointment fell off the top-5 by
    // importance, 23-07-2026). Budgeted surfaces (session-start greet, MCP bootstrap) pass their
    // own explicit small limits.
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!(m.reminders(req.limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT))?))).await
}

async fn latest_save_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    with_tenant(&st, &headers, false, |m, _agent| Ok(json!({ "latest_save_ms": m.latest_save_ms()? }))).await
}

async fn history_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<HistoryReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!(m.history(&req.uri, req.limit.unwrap_or(20).min(MAX_LIMIT))?))).await
}

async fn forget_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ForgetReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!({ "forgotten": m.forget(&req.uri)? }))).await
}

async fn remember_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RememberReq>) -> ApiResp {
    // client_err: a bad valid interval (valid_to <= valid_from) is client input -> 400, not 500.
    with_tenant(&st, &headers, true, move |m, agent| {
        Ok(json!({ "uri": m.remember(&req.text, ns_or(&req.namespace, "resources/notes"), req.valid_from, req.valid_to, agent.as_deref())? }))
    })
    .await
}

async fn invalidate_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<InvalidateReq>) -> ApiResp {
    // client_err: a non-positive cut is client input -> 400 (consistent with the other write
    // handlers); genuine storage faults are rare and accept the same generic 400 those do.
    with_tenant(&st, &headers, true, move |m, _agent| {
        Ok(json!({ "invalidated": m.invalidate(&req.uri, req.valid_to)? }))
    })
    .await
}

async fn link_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<LinkReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, _agent| {
        m.link(&req.from, &req.to, &req.rel)?;
        Ok(json!({ "linked": 1 }))
    })
    .await
}

async fn unlink_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<LinkReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, _agent| {
        Ok(json!({ "unlinked": m.unlink(&req.from, &req.to, &req.rel)? }))
    })
    .await
}

async fn edges_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<EdgesReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!(m.edges_of(&req.uri)?))).await
}

async fn edges_all_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<EdgesAllReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| Ok(json!(m.all_edges(req.limit.unwrap_or(5000).min(50_000))?))).await
}

async fn neighbors_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<NeighborsReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| {
        Ok(json!(m.neighbors(&req.seeds, req.depth.unwrap_or(1).min(5), req.limit.unwrap_or(50).min(MAX_LIMIT))?))
    })
    .await
}

async fn recall_expanded_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecallExpandedReq>) -> ApiResp {
    with_tenant(&st, &headers, false, move |m, _agent| {
        // Split shape so clients can render the graph's contribution distinguishably; older
        // clients that expected a flat array must upgrade alongside the server (single-admin
        // deployment; the hook path degrades to plain /recall on a decode failure).
        // `riders` (scored, with hop/via/rel provenance) and `links` (edges internal to the
        // result set) are additive: pre-provenance clients keep reading seeds+neighbors.
        let g =
            m.recall_expanded_graph(&req.query, req.limit.unwrap_or(6).min(MAX_LIMIT), req.depth.unwrap_or(1).min(5))?;
        let neighbors: Vec<&crate::entry::Entry> = g.riders.iter().map(|r| &r.entry).collect();
        Ok(json!({ "seeds": g.seeds, "neighbors": neighbors, "riders": g.riders, "links": g.links }))
    })
    .await
}

/// As `with_tenant`, but only for FULL-TENANT identities: batch graph maintenance under a
/// scoped identity would rebuild/prune from a partial view of the store and damage the graph.
async fn with_tenant_unscoped<F>(st: &AppState, headers: &HeaderMap, client_err: bool, f: F) -> ApiResp
where
    F: FnOnce(&LocalMemory, Option<String>) -> Result<serde_json::Value> + Send + 'static,
{
    if let Some(id) = resolve_identity(st, headers) {
        if id.tenant.is_some() && !id.scope_unbound() {
            return err(StatusCode::FORBIDDEN, "full-tenant token required for this operation");
        }
    }
    with_tenant(st, headers, client_err, f).await
}

async fn reindex_links_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    with_tenant_unscoped(&st, &headers, true, move |m, _agent| {
        let (linked, pruned) = m.reindex_links()?;
        Ok(json!({ "linked": linked, "pruned": pruned }))
    })
    .await
}

#[derive(Deserialize)]
struct ReindexMentionsReq {
    #[serde(default)]
    dry_run: bool,
}

async fn reindex_mentions_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ReindexMentionsReq>) -> ApiResp {
    with_tenant_unscoped(&st, &headers, true, move |m, _agent| {
        let (found, added) = m.reindex_mentions(req.dry_run)?;
        Ok(json!({ "found": found, "added": added }))
    })
    .await
}

async fn decision_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<DecisionReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        let ns = ns_or(&req.namespace, "resources/notes");
        Ok(json!({ "uri": m.log_decision(&req.title, &req.context, &req.decision, &req.rationale, ns, agent.as_deref())? }))
    })
    .await
}

async fn lesson_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<LessonReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        Ok(json!({ "uri": m.log_lesson(&req.title, &req.lesson, ns_or(&req.namespace, "agent/lessons"), agent.as_deref())? }))
    })
    .await
}

async fn incident_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<IncidentReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        let ns = ns_or(&req.namespace, "resources/incidents");
        Ok(json!({ "uri": m.log_incident(&req.title, &req.impact, &req.resolution, ns, agent.as_deref())? }))
    })
    .await
}

async fn runbook_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RunbookReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        Ok(json!({ "uri": m.log_runbook(&req.title, &req.steps, ns_or(&req.namespace, "resources/runbooks"), agent.as_deref())? }))
    })
    .await
}

async fn convention_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ConventionReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        Ok(json!({ "uri": m.log_convention(&req.title, &req.rule, ns_or(&req.namespace, "resources/conventions"), agent.as_deref())? }))
    })
    .await
}

async fn reminder_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ReminderReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, agent| {
        Ok(json!({ "uri": m.add_reminder(&req.title, &req.text, ns_or(&req.namespace, "agent/reminders"), agent.as_deref())? }))
    })
    .await
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
    /// original importance (migration); absent = kind default
    #[serde(default)]
    importance: Option<i64>,
}

async fn import_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ImportReq>) -> ApiResp {
    with_tenant(&st, &headers, true, move |m, _agent| {
        let kind = crate::entry::Kind::from_str(&req.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", req.kind))?;
        let ns = if req.namespace.is_empty() { "resources/notes" } else { &req.namespace };
        let uri = if req.created_ms > 0 || req.importance.is_some() {
            m.import_record_at(kind, ns, &req.title, &req.body, req.created_ms, req.importance)?
        } else {
            m.import_record(kind, ns, &req.title, &req.body)?
        };
        Ok(json!({ "uri": uri }))
    })
    .await
}

// --- admin (IAM) routes: require the root admin token ---

#[derive(Deserialize)]
struct AdminAddReq {
    tenant: String,
    #[serde(default)]
    display: String,
    #[serde(default)]
    label: String,
    /// Optional per-agent identity label for the minted token (None = agent-less).
    #[serde(default)]
    agent: Option<String>,
    /// Scope primitive (all optional; omitting every one mints a full-tenant token exactly
    /// as before). `scope_read`: scopes this token may read (global always included).
    /// `scope_write`: the single scope stamped on its writes (omit = read-only when any
    /// scope field is set). `adapter`: may assert the reader's scopes per request (Q1).
    #[serde(default)]
    scope_read: Option<Vec<String>>,
    #[serde(default)]
    scope_write: Option<String>,
    #[serde(default)]
    adapter: bool,
}

#[derive(Deserialize)]
struct AdminTargetReq {
    target: String,
}

async fn admin_add_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminAddReq>) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        let scoped = req.scope_read.is_some() || req.scope_write.is_some() || req.adapter;
        let (tenant, token) = if scoped {
            let t = crate::config::canonical_tenant(&req.tenant);
            let tok = iam.mint_scoped_token(
                &req.tenant,
                &req.label,
                req.agent.as_deref(),
                req.scope_read.as_deref().unwrap_or(&[]),
                req.scope_write.as_deref(),
                req.adapter,
            )?;
            (t, tok)
        } else {
            iam.create_tenant(&req.tenant, &req.display, &req.label, req.agent.as_deref())?
        };
        Ok(json!({ "tenant": tenant, "token": token, "agent": req.agent.as_deref().and_then(crate::config::canonical_agent) }))
    })
}

async fn admin_list_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        let rows: Vec<_> = iam
            .list()?
            .into_iter()
            .map(|(t, s, n, agents)| json!({ "tenant": t, "status": s, "tokens": n, "agents": agents }))
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

/// Assemble the router. `/healthz` is open; every other route requires a valid bearer token. `iam`
/// is the startup-opened IAM handle (None if it could not be opened), shared for token resolution;
/// the per-tenant memory cache starts empty and fills on first use.
pub fn router(auth: Arc<dyn Authenticator>, iam: Option<crate::iam::Iam>) -> Router {
    let state = AppState {
        auth,
        iam: Arc::new(Mutex::new(iam)),
        mem: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/recall", post(recall_h))
        .route("/recent", post(recent_h))
        .route("/persona", post(persona_h))
        .route("/reminders", post(reminders_h))
        .route("/latest_save", post(latest_save_h))
        .route("/history", post(history_h))
        .route("/forget", post(forget_h))
        .route("/remember", post(remember_h))
        .route("/invalidate", post(invalidate_h))
        .route("/link", post(link_h))
        .route("/unlink", post(unlink_h))
        .route("/edges", post(edges_h))
        .route("/edges_all", post(edges_all_h))
        .route("/neighbors", post(neighbors_h))
        .route("/recall_expanded", post(recall_expanded_h))
        .route("/reindex_links", post(reindex_links_h))
        .route("/reindex_mentions", post(reindex_mentions_h))
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
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
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
        // the private key is a secret: 0600, unlike the (public) cert beside it
        let _ = crate::config::write_secret(&tdir.join("key.pem"), &key_pem);
        eprintln!("dmem serve: generated self-signed cert at {}", cpath.display());
        eprintln!("           clients: set `ca_cert` to that file (or `insecure = true`)");
    }
    Ok((cert_pem, key_pem))
}

/// Bind `addr` and serve. With TLS (cert/key or generate) it serves HTTPS; otherwise plain
/// HTTP with a loud warning. Tokens come from the environment.
/// Operator overrides for the fail-closed startup defaults (security audit 11-08-2026).
#[derive(Default)]
pub struct HardeningOpts {
    pub allow_insecure_http: bool,
    pub allow_env_only: bool,
}

/// Loopback = 127.0.0.0/8, ::1, or the literal `localhost` host part.
fn is_loopback_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr).trim_matches(['[', ']']);
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

pub fn run_blocking(addr: &str, tls: TlsOpts, hardening: HardeningOpts) -> Result<()> {
    // Fail closed on a cleartext public bind (audit High #2): a non-loopback address without
    // TLS puts bearer tokens and memory content on the wire. Loopback keeps the old default.
    // A half-configured pair is a misconfiguration, not "no TLS": fail loudly instead of
    // silently serving plain HTTP (2nd-opinion follow-up: `--tls-cert` alone previously
    // counted as "has TLS" for the bind guard while the serve path fell through to HTTP).
    if tls.cert.is_some() != tls.key.is_some() {
        anyhow::bail!("--tls-cert and --tls-key must be given together (or use --tls-generate)");
    }
    let has_tls = (tls.cert.is_some() && tls.key.is_some()) || tls.generate;
    if !is_loopback_addr(addr) && !has_tls && !hardening.allow_insecure_http {
        anyhow::bail!(
            "refusing to serve plain HTTP on non-loopback {addr}: add --tls-cert/--tls-key or \
             --tls-generate, or override explicitly with --allow-insecure-http"
        );
    }
    // rustls 0.23 needs a process-wide crypto provider installed before any TLS work.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let auth = BearerAuth::from_env()?;
    // IAM: open ONCE here (shared by the router for token resolution), ensure a bootstrap root
    // admin token, and print it once if newly generated. If IAM cannot be opened, run env-only and
    // say so loudly: token revocation is then not enforced until IAM recovers (no silent per-request
    // open swallowing the failure as before).
    let iam = match crate::iam::Iam::open() {
        Ok(iam) => {
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
            Some(iam)
        }
        Err(e) if hardening.allow_env_only => {
            eprintln!("dmem serve: IAM unavailable ({e:#}); serving with env tokens only (--allow-env-only) - token revocation/suspension is NOT enforced until IAM is reachable.");
            None
        }
        // Fail closed (audit Medium #8): without the explicit override, an unopenable IAM db
        // means revoked tokens could come back to life via the env fallback - refuse instead.
        Err(e) => anyhow::bail!(
            "IAM database unavailable ({e:#}); refusing to start (revocation would not be \
             enforced). Fix the IAM db, or start with --allow-env-only to accept that risk."
        ),
    };
    if auth.is_empty() {
        eprintln!(
            "dmem serve: tip - create tenants with the admin token (`dmem admin add <tenant>`), \
             or set DM_TOKEN_<tenant>=<secret> for a quick static token."
        );
    }
    // Warm the process-wide embedder before serving so the FIRST recall does not pay the model
    // load on a request. No-op without the vector feature.
    #[cfg(feature = "zvec")]
    crate::tools::warm_embedder();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let app = router(Arc::new(auth), iam);
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
    #[test]
    fn loopback_addr_detection() {
        for ok in ["127.0.0.1:8077", "127.9.9.9:80", "localhost:8077", "[::1]:8077"] {
            assert!(is_loopback_addr(ok), "{ok} is loopback");
        }
        for bad in ["0.0.0.0:8077", "10.100.30.64:8077", "192.168.217.1:8077", "memory.example.com:443"] {
            assert!(!is_loopback_addr(bad), "{bad} is not loopback");
        }
    }

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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DM_TOKEN_ACME", "secret123-0123456789");
        let a = BearerAuth::from_env().unwrap();
        let id = a.identity_for(Some("Bearer secret123-0123456789")).expect("token resolves");
        assert_eq!(id.tenant.as_deref(), Some("acme"));
        assert!(id.agent.is_none(), "plain DM_TOKEN_<TENANT> stays agent-less");
        assert!(!id.is_admin);
        // case-insensitive scheme
        assert_eq!(a.identity_for(Some("bearer secret123-0123456789")).unwrap().tenant.as_deref(), Some("acme"));
        assert!(a.identity_for(Some("Bearer nope")).is_none());
        assert!(a.identity_for(Some("Basic secret123-0123456789")).is_none());
        assert!(a.identity_for(None).is_none());
        std::env::remove_var("DM_TOKEN_ACME");
    }

    #[test]
    fn env_token_agent_forms_parse() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DM_TOKEN_WAKSPACE__IZU", "agtok1-0123456789abc"); // tenant + agent
        std::env::set_var("DM_TOKEN_MY_TENANT__SHESTA", "agtok2-0123456789abc"); // single underscores stay in the tenant
        std::env::set_var("DM_TOKEN_EDGE__", "agtok3-0123456789abc"); // trailing __ = agent-less
        std::env::set_var("DM_TOKEN___GHOST", "agtok4-0123456789abc"); // empty tenant = ignored
        std::env::set_var("DM_TOKEN_T2__A__B", "agtok5-0123456789abc"); // FIRST __ splits; the rest is the agent
        let a = BearerAuth::from_env().unwrap();

        let id = a.identity_for(Some("Bearer agtok1-0123456789abc")).expect("agent token resolves");
        assert_eq!(id.tenant.as_deref(), Some("wakspace"));
        assert_eq!(id.agent.as_deref(), Some("izu"), "env agent label is canonicalized (lowercase)");
        assert!(!id.is_admin);

        let id = a.identity_for(Some("Bearer agtok2-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("my_tenant"), "single underscores belong to the tenant");
        assert_eq!(id.agent.as_deref(), Some("shesta"));

        let id = a.identity_for(Some("Bearer agtok3-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("edge"));
        assert!(id.agent.is_none(), "trailing __ means agent-less");

        assert!(a.identity_for(Some("Bearer agtok4-0123456789abc")).is_none(), "empty tenant is skipped");

        let id = a.identity_for(Some("Bearer agtok5-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("t2"));
        assert_eq!(id.agent.as_deref(), Some("a__b"), "only the first __ splits");

        for k in ["DM_TOKEN_WAKSPACE__IZU", "DM_TOKEN_MY_TENANT__SHESTA", "DM_TOKEN_EDGE__", "DM_TOKEN___GHOST", "DM_TOKEN_T2__A__B"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn duplicate_secret_to_different_tenants_fails_fast() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DM_TOKEN_ACME", "shared-0123456789abc");
        std::env::set_var("DM_TOKEN_GLOBEX", "shared-0123456789abc");
        let r = BearerAuth::from_env();
        assert!(r.is_err(), "same secret -> two tenants must be rejected");
        std::env::remove_var("DM_TOKEN_ACME");
        std::env::remove_var("DM_TOKEN_GLOBEX");
    }

    #[test]
    fn duplicate_secret_to_different_agents_fails_fast() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DM_TOKEN_ACME2__IZU", "shared2-0123456789ab");
        std::env::set_var("DM_TOKEN_ACME2__DEVIN", "shared2-0123456789ab");
        let r = BearerAuth::from_env();
        assert!(r.is_err(), "same secret -> same tenant but two agents must be rejected (attribution)");
        std::env::remove_var("DM_TOKEN_ACME2__IZU");
        std::env::remove_var("DM_TOKEN_ACME2__DEVIN");
    }

    #[test]
    fn memory_cache_reuses_one_handle_per_tenant() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmcache-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        let st = AppState {
            auth: Arc::new(BearerAuth::from_env().unwrap()),
            iam: Arc::new(Mutex::new(None)),
            mem: Arc::new(Mutex::new(HashMap::new())),
        };
        let a1 = st.memory_for("tenant_a").unwrap();
        let a2 = st.memory_for("tenant_a").unwrap();
        let b1 = st.memory_for("tenant_b").unwrap();
        assert!(Arc::ptr_eq(&a1, &a2), "same tenant must reuse the cached handle");
        assert!(!Arc::ptr_eq(&a1, &b1), "different tenants must get different handles");
        std::env::remove_var("DM_DATA_DIR");
    }

    #[tokio::test]
    async fn recall_route_authorizes_and_returns_hits() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmsrv-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_T1SRV", "tok1-0123456789abcde");
        // seed a record into tenant t1srv
        let m = Memory::open_tenant("t1srv").unwrap();
        m.remember("the vector substrate is zvec", "resources/notes", None, None, None).unwrap();

        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

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
                    .header("authorization", "Bearer tok1-0123456789abcde")
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

    // Full save->recall round-trip over the HTTP API (not just a pre-seeded store): POST /remember,
    // then POST /recall and assert the just-saved record comes back. Closes the integration gap that
    // unit tests over the in-process store do not cover.
    #[tokio::test]
    async fn remember_then_recall_round_trip_over_http() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmrt-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_RT1", "rttok-0123456789abcd");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

        // POST /remember (write through the wire)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/remember")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer rttok-0123456789abcd")
                    .body(Body::from(r#"{"text":"the mail relay runs postfix on local raid","namespace":"resources/notes"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("daimon://"), "remember should return the saved uri: {s}");

        // POST /recall finds it (read back through the wire)
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/recall")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer rttok-0123456789abcd")
                    .body(Body::from(r#"{"query":"postfix mail relay","limit":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("postfix"), "recall should return the just-saved record: {s}");

        std::env::remove_var("DM_TOKEN_RT1");
        std::env::remove_var("DM_DATA_DIR");
    }

    // The scope primitive over the wire: scoped tokens stamp writes and read a filtered
    // slice; read-only tokens cannot write; adapter tokens assert scopes per request;
    // batch graph maintenance is full-tenant only. One test, one shared router - the
    // scenarios build on each other's data.
    #[tokio::test]
    async fn scoped_tokens_over_http_stamp_filter_and_guard() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmscope-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::fs::create_dir_all(&dir).unwrap();
        let iam = crate::iam::Iam::open_at(&dir.join("iam.db")).unwrap();
        let (_t, full) = iam.create_tenant("scopetest", "", "", None).unwrap();
        let user_a = iam
            .mint_scoped_token("scopetest", "a", None, &["user:a".to_string()], Some("user:a"), false)
            .unwrap();
        let readonly = iam.mint_scoped_token("scopetest", "ro", None, &["user:a".to_string()], None, false).unwrap();
        let bridge = iam.mint_scoped_token("scopetest", "bridge", None, &[], None, true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));

        let call = |app: Router, uri: &'static str, tok: String, body: String, hdrs: Vec<(&'static str, String)>| async move {
            let mut b = Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {tok}"));
            for (k, v) in hdrs {
                b = b.header(k, v);
            }
            let resp = app.oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            (status, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or(serde_json::Value::Null))
        };

        // full-tenant token writes a global record
        let (st, _) = call(
            app.clone(),
            "/remember",
            full.clone(),
            r#"{"text":"global fact for everyone","namespace":"resources/notes"}"#.into(),
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        // scoped token writes - stamped user:a regardless of payload (no scope field exists on the wire)
        let (st, v) = call(
            app.clone(),
            "/remember",
            user_a.clone(),
            r#"{"text":"private note of user a","namespace":"resources/notes"}"#.into(),
            vec![],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let a_uri = v.get("uri").and_then(|u| u.as_str()).unwrap().to_string();
        // full token sees the stored scope stamp
        let (_, v) = call(app.clone(), "/recall", full.clone(), r#"{"query":"private note user","limit":5}"#.into(), vec![]).await;
        let hit = v.as_array().and_then(|a| a.iter().find(|e| e["uri"].as_str() == Some(a_uri.as_str()))).cloned();
        assert_eq!(hit.expect("full token sees it")["scope"], "user:a", "write stamped from the token");

        // scoped token sees global + own; a different-scope record stays invisible
        let (_, v) = call(app.clone(), "/recall", user_a.clone(), r#"{"query":"global fact everyone","limit":5}"#.into(), vec![]).await;
        assert!(v.as_array().is_some_and(|a| !a.is_empty()), "global rides along: {v}");

        // read-only scoped token: write is rejected
        let (st, _) = call(
            app.clone(),
            "/remember",
            readonly.clone(),
            r#"{"text":"should never land","namespace":"resources/notes"}"#.into(),
            vec![],
        )
        .await;
        assert_ne!(st, StatusCode::OK, "read-only token cannot write (Q3)");

        // scoped token cannot forget a global record... (mutation guard)
        let (_, v) = call(app.clone(), "/recall", full.clone(), r#"{"query":"global fact everyone","limit":1}"#.into(), vec![]).await;
        let g_uri = v[0]["uri"].as_str().unwrap().to_string();
        let (st, _) = call(app.clone(), "/forget", user_a.clone(), format!(r#"{{"uri":"{g_uri}"}}"#), vec![]).await;
        assert_ne!(st, StatusCode::OK, "scoped token cannot retract a global record");
        // ...and cannot run batch graph maintenance
        let (st, _) = call(app.clone(), "/reindex_links", user_a.clone(), "{}".into(), vec![]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "reindex is full-tenant only");

        // adapter: no headers = global-only read, and write rejected
        let (_, v) = call(app.clone(), "/recall", bridge.clone(), r#"{"query":"private note user","limit":5}"#.into(), vec![]).await;
        assert!(
            v.as_array().is_some_and(|a| a.iter().all(|e| e["uri"].as_str() != Some(a_uri.as_str()))),
            "adapter without asserted scopes must not see user:a: {v}"
        );
        let (st, _) = call(app.clone(), "/remember", bridge.clone(), r#"{"text":"x","namespace":"resources/notes"}"#.into(), vec![]).await;
        assert_ne!(st, StatusCode::OK, "adapter write without X-DM-Write-Scope is rejected");
        // adapter asserting the scope reads it; asserting a write scope stamps it
        let (_, v) = call(
            app.clone(),
            "/recall",
            bridge.clone(),
            r#"{"query":"private note user","limit":5}"#.into(),
            vec![("x-dm-scopes", "user:a".into())],
        )
        .await;
        assert!(
            v.as_array().is_some_and(|a| a.iter().any(|e| e["uri"].as_str() == Some(a_uri.as_str()))),
            "adapter with asserted scope sees it: {v}"
        );
        let (st, v) = call(
            app.clone(),
            "/remember",
            bridge.clone(),
            r#"{"text":"room record via the bridge","namespace":"resources/notes"}"#.into(),
            vec![("x-dm-write-scope", "room:z".into())],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let room_uri = v["uri"].as_str().unwrap().to_string();
        let (_, v) = call(app.clone(), "/recall", full.clone(), r#"{"query":"room record bridge","limit":5}"#.into(), vec![]).await;
        let hit = v.as_array().and_then(|a| a.iter().find(|e| e["uri"].as_str() == Some(room_uri.as_str()))).cloned();
        assert_eq!(hit.expect("stored")["scope"], "room:z", "adapter write stamped from its asserted scope");

        std::env::remove_var("DM_DATA_DIR");
    }

    // /recall_expanded returns the graph-provenance shape: seeds + neighbors (pre-provenance
    // clients) + riders (hop/via/rel/score) + links (edges internal to the result set).
    #[tokio::test]
    async fn recall_expanded_returns_riders_and_links_over_http() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmrx-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_RX1", "rxtok-0123456789abcd");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

        let post = |uri: &'static str, body: &'static str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", "Bearer rxtok-0123456789abcd")
                .body(Body::from(body))
                .unwrap()
        };
        let resp = app
            .clone()
            .oneshot(post("/remember", r#"{"text":"epsilon anchor matches the query","namespace":"resources/notes"}"#))
            .await
            .unwrap();
        let seed_uri: String = {
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            v.get("uri").and_then(|u| u.as_str()).unwrap_or_default().to_string()
        };
        // The rider must be far from the query in BOTH channels: zero token overlap for the
        // keyword path AND semantically unrelated for the hybrid/candle path - a "connected
        // rider" phrasing scored close enough in cosine to surface as a SEED under --features
        // dist, emptying the riders array (caught at the 0.3.0 release gate).
        let resp = app
            .clone()
            .oneshot(post("/remember", r#"{"text":"midnight watering schedule for the rooftop garden","namespace":"resources/notes"}"#))
            .await
            .unwrap();
        let rider_uri: String = {
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            v.get("uri").and_then(|u| u.as_str()).unwrap_or_default().to_string()
        };
        let link_body = format!(r#"{{"from":"{seed_uri}","to":"{rider_uri}","rel":"informed"}}"#);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/link")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer rxtok-0123456789abcd")
                    .body(Body::from(link_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(post("/recall_expanded", r#"{"query":"epsilon anchor matches","limit":3,"depth":1}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("seeds").and_then(|s| s.as_array()).is_some_and(|a| !a.is_empty()), "seeds present: {v}");
        let riders = v.get("riders").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        assert!(
            riders.iter().any(|r| r.pointer("/entry/uri").and_then(|u| u.as_str()) == Some(rider_uri.as_str())),
            "rider present with entry provenance: {v}"
        );
        assert!(
            riders.iter().all(|r| r.get("hop").is_some() && r.get("via").is_some() && r.get("score").is_some()),
            "riders carry hop/via/score: {v}"
        );
        assert!(
            v.get("links").and_then(|l| l.as_array()).is_some_and(|a| a
                .iter()
                .any(|e| e.get("rel").and_then(|r| r.as_str()) == Some("informed"))),
            "internal edge present in links: {v}"
        );
        assert!(
            v.get("neighbors").and_then(|n| n.as_array()).is_some_and(|a| !a.is_empty()),
            "pre-provenance neighbors field kept for old clients: {v}"
        );

        std::env::remove_var("DM_TOKEN_RX1");
        std::env::remove_var("DM_DATA_DIR");
    }

    // The identity-bleed regression this branch exists for: an agent token gets its OWN persona
    // plus shared governance over the wire, never another agent's; an agent-less token on the
    // SAME tenant still sees the legacy full set.
    // ENV_LOCK must span the awaits (the env vars must stay set for the whole request), same as
    // the other route tests here; silence the lint instead of growing its baseline count.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn persona_route_serves_the_tokens_agent_only() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmpers-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_PT1__IZU", "izutok-0123456789abc");
        std::env::set_var("DM_TOKEN_PT1", "plaintok-0123456789a");
        let m = Memory::open_tenant("pt1").unwrap();
        m.import_record(crate::entry::Kind::Persona, "agents/izu/persona", "Izu Persona", "I am Izu").unwrap();
        m.import_record(crate::entry::Kind::Persona, "agents/shesta/persona", "Shesta Persona", "I am Shesta").unwrap();
        m.import_record(crate::entry::Kind::Persona, "shared/governance", "House Rules", "shared rules").unwrap();

        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);
        let call = |tok: &str| {
            let app = app.clone();
            let tok = format!("Bearer {tok}");
            async move {
                let resp = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/persona")
                            .header("authorization", tok)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                String::from_utf8_lossy(&body).to_string()
            }
        };

        let s = call("izutok-0123456789abc").await;
        assert!(s.contains("Izu Persona") && s.contains("House Rules"), "own persona + shared governance: {s}");
        assert!(!s.contains("Shesta Persona"), "another agent's persona must not leak: {s}");

        let s = call("plaintok-0123456789a").await;
        assert!(s.contains("Izu Persona") && s.contains("Shesta Persona"), "agent-less token keeps legacy behaviour: {s}");

        std::env::remove_var("DM_TOKEN_PT1__IZU");
        std::env::remove_var("DM_TOKEN_PT1");
        std::env::remove_var("DM_DATA_DIR");
    }

    // Write attribution over the wire: a save through an agent token comes back from recall
    // with the author:<agent> tag stamped by the server (the client sends no attribution).
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the awaits, as above
    #[tokio::test]
    async fn remember_with_agent_token_stamps_author() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dmattr-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_AT1__SHESTA", "shestatok-0123456789");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/remember")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer shestatok-0123456789")
                    .body(Body::from(r#"{"text":"the report template lives in projects docs","namespace":"resources/notes"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/recall")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer shestatok-0123456789")
                    .body(Body::from(r#"{"query":"report template docs","limit":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("author:shesta"), "recalled record carries the token's agent as author: {s}");

        std::env::remove_var("DM_TOKEN_AT1__SHESTA");
        std::env::remove_var("DM_DATA_DIR");
    }
}
