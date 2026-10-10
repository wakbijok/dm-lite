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

use crate::tools::{EmbedPlan, LocalMemory, Memory};
use anyhow::Result;
use axum::{
    body::Bytes,
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

    /// The tenants this authenticator's tokens resolve to, when it can list them (the env
    /// tokens can). `dmem serve` opens these at start-up to judge readiness.
    fn tenants(&self) -> Vec<String> {
        Vec::new()
    }
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

    fn tenants(&self) -> Vec<String> {
        let mut t: Vec<String> = self.map.values().map(|(tenant, _)| tenant.clone()).collect();
        t.sort();
        t.dedup();
        t
    }
}

/// Shared, per-tenant engine handle. rusqlite Connection is Send but !Sync, so each tenant's
/// engine is behind its own Mutex; the IAM connection (also !Sync) sits behind one Mutex. Held
/// as the `Memory` enum (always the `Local` variant here) so the transport-agnostic MCP
/// dispatch can borrow it directly; the REST handlers borrow the inner engine.
type TenantHandle = Arc<Mutex<Memory>>;

/// One tenant's place in the engine cache: None until the tenant's first open finishes. Its own
/// lock serializes that open (a first open can run the vector-slot migration, which takes a
/// while), so concurrent first requests for the same tenant wait for one open instead of
/// racing two, while every other tenant is served.
type TenantSlot = Arc<Mutex<Option<TenantHandle>>>;

/// A server tenant is always the local engine (`open_tenant` never yields a remote client).
fn local_ref(m: &Memory) -> &LocalMemory {
    match m {
        Memory::Local(l) => l,
        #[cfg(feature = "client")]
        Memory::Remote(_) => unreachable!("server tenant handles are always local"),
    }
}

fn local_mut(m: &mut Memory) -> &mut LocalMemory {
    match m {
        Memory::Local(l) => l,
        #[cfg(feature = "client")]
        Memory::Remote(_) => unreachable!("server tenant handles are always local"),
    }
}

#[derive(Clone)]
struct AppState {
    auth: Arc<dyn Authenticator>,
    /// The IAM connection, opened ONCE at startup (None if it could not be opened). Token
    /// resolution locks it briefly; no per-request open.
    iam: Arc<Mutex<Option<crate::iam::Iam>>>,
    /// Per-tenant engine cache: a request reuses the tenant's open SQLite/zvec handles instead of
    /// re-opening them every call. zvec's Collection is Send + Sync, so this is safe to share.
    /// The map lock is held only to find a tenant's slot, never across an open.
    mem: Arc<Mutex<HashMap<String, TenantSlot>>>,
    /// Whether each opened tenant has a working vector index, recorded when it opens and when
    /// a retry opens it later, so readiness and the pre-embed step never need a tenant lock.
    vectors: Arc<Mutex<HashMap<String, bool>>>,
    /// Tenants without a vector index, and when their index is next retried (see
    /// `AppState::retry_vector_indexes`): a cause that goes away heals without a restart.
    vector_retry: Arc<Mutex<HashMap<String, Retry>>>,
    /// The first retry's wait, doubled after each failed retry up to `VECTOR_RETRY_MAX`.
    vector_retry_base: Duration,
    /// The start-up pass (vector slot migration, then opening every known tenant) is done;
    /// until then `/readyz` is not ready, so a fresh server is judged before it takes traffic.
    started: Arc<AtomicBool>,
    /// The readiness checks that touch the disk (IAM query, data dir probe), cached for
    /// `io_check_ttl` so a probe loop cannot turn into an fsync loop; one caller refreshes
    /// them while the others wait for its result.
    io_checks: Arc<Mutex<Option<IoChecks>>>,
    io_check_ttl: Duration,
    /// Bounds concurrent embedding (separate permits for record bodies and recall queries).
    #[cfg(feature = "zvec")]
    embed_gate: Arc<EmbedGate>,
    /// The embedder every tenant handle is opened with, and the one a request pre-embeds its
    /// texts with before it takes the tenant lock (the same instance, so the vectors agree).
    #[cfg(feature = "zvec")]
    embedder: Arc<dyn crate::embedder::Embedder>,
    /// Requests being handled right now (every route).
    in_flight: Arc<AtomicUsize>,
    /// A stop signal arrived: no new connections, in-flight requests are finishing.
    draining: Arc<AtomicBool>,
    /// The drain is over and the tenants are being flushed and closed: no request may take a
    /// tenant lock or open a tenant any more.
    closing: Arc<AtomicBool>,
    /// SHA-256 of `DM_METRICS_TOKEN`: a bearer that may read `/metrics` and nothing else (the
    /// admin token may too).
    metrics_token: Option<String>,
    /// The last not-ready summary logged by `/readyz` (None = ready), so a probe every few
    /// seconds logs a change once instead of every time.
    readiness_note: Arc<Mutex<Option<String>>>,
}

/// One readiness check's outcome: Ok(what is fine), Err(why not ready). Neither names a tenant,
/// a path or a secret: `/readyz` answers anyone who can reach the port.
type Check = std::result::Result<String, String>;

/// The `/readyz` verdict: each check, plus details for the server log only (tenant names).
struct Readiness {
    checks: Vec<(&'static str, Check)>,
    detail: Vec<String>,
}

impl Readiness {
    fn ready(&self) -> bool {
        self.checks.iter().all(|(_, c)| c.is_ok())
    }

    fn failing(&self) -> Vec<&'static str> {
        self.checks.iter().filter(|(_, c)| c.is_err()).map(|(n, _)| *n).collect()
    }
}

/// The disk-touching readiness checks as last computed (`AppState::io_checks`).
#[derive(Clone)]
struct IoChecks {
    at: Instant,
    iam: Check,
    data_dir: Check,
    /// Details for the server log only.
    detail: Vec<String>,
}

/// How long the disk-touching readiness checks are reused (`/readyz` answers anyone who can
/// reach the port, and each fresh check writes and syncs a probe file).
const IO_CHECK_TTL: Duration = Duration::from_secs(2);

/// When a tenant without a vector index is next retried, the wait after a failed retry, and
/// since when (epoch ms) it has been without one: saves from then on have no vector.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
struct Retry {
    next: Instant,
    delay: Duration,
    since_ms: i64,
}

/// What one retry of a tenant's vector index found (`AppState::reopen_for_vectors`).
#[cfg(feature = "zvec")]
enum Reopened {
    /// Still without a vector index.
    Not,
    /// Nothing to do any more (already healed, or the tenant's database is gone).
    Resolved,
    /// The index opened and its handle is in the cache now: the keyword-only handle it replaced
    /// (requests may still hold it), the index, and since when saves went without a vector.
    Healed { old: Option<TenantHandle>, index: Arc<crate::zvec_index::ZvecIndex>, since_ms: i64 },
}

/// How long the re-embedding after a heal waits for requests still on the replaced
/// keyword-only handle (their saves have no vector either) before it starts anyway.
#[cfg(feature = "zvec")]
const HEAL_DRAIN: Duration = Duration::from_secs(60);

/// The first retry of a tenant's vector index waits this long; each failed retry doubles it.
const VECTOR_RETRY_BASE: Duration = Duration::from_secs(5);

/// The longest wait between two retries of one tenant's vector index.
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
const VECTOR_RETRY_MAX: Duration = Duration::from_secs(300);

/// The wait after a failed retry: double the last one, at least `base`, at most the cap.
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
fn next_retry_delay(last: Duration, base: Duration) -> Duration {
    (last * 2).max(base).min(VECTOR_RETRY_MAX)
}

/// A counting semaphore for blocking callers (the pre-embed step runs on the blocking pool).
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
struct Permits {
    free: Mutex<usize>,
    freed: Condvar,
    total: usize,
}

/// One held permit, returned on drop.
struct Permit<'a>(&'a Permits);

#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
impl Permits {
    fn new(n: usize) -> Self {
        let n = n.max(1);
        Self { free: Mutex::new(n), freed: Condvar::new(), total: n }
    }

    /// Wait for a permit: (the permit, how long it took).
    fn acquire(&self) -> (Permit<'_>, Duration) {
        let t = Instant::now();
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|p| p.into_inner());
        }
        *free -= 1;
        (Permit(self), t.elapsed())
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

/// Bounds the embedding a server runs at once. Embedding moved out of the tenant lock (P0-5),
/// which had also been its only limit (one embed per tenant at a time); each forward pass
/// allocates tens of MB and a save can take 64 of them, so without a bound a burst of long
/// saves could exhaust the memory of a small pod. Record bodies (`DM_EMBED_CONCURRENCY`) and
/// recall queries (`DM_EMBED_QUERY_CONCURRENCY`) have separate permits, so a recall never
/// queues behind long saves. Texts the engine embeds itself under the tenant lock (a re-embed
/// after an invalidation, the slot migration) take no permit: the lock already allows one per
/// tenant.
#[cfg(feature = "zvec")]
struct EmbedGate {
    saves: Permits,
    queries: Permits,
}

/// A request that reached the tenant after shutdown started closing the tenants (503).
#[derive(Debug)]
struct ShuttingDown;

impl std::fmt::Display for ShuttingDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server is shutting down")
    }
}

impl std::error::Error for ShuttingDown {}

/// What closing the tenants at shutdown did (`AppState::close_tenants`).
#[derive(Debug, Default)]
struct CloseReport {
    /// Tenants whose vector index was flushed.
    flushed: usize,
    /// Tenants closed (flushed or without an index).
    closed: usize,
    /// Tenants still in their first open (the start-up slot migration; it resumes next start).
    opening: Vec<String>,
    /// Tenants whose lock a request still held at the deadline.
    busy: Vec<String>,
    /// Tenants whose flush failed, with the error.
    failed: Vec<(String, String)>,
}

impl AppState {
    /// The tenant's explicit-write-scope policy, read from IAM on every request (so an admin
    /// change applies to the next request). Env-only serving (IAM unavailable) has no policy
    /// store: false, which the startup warning for that mode says.
    fn requires_explicit_write_scope(&self, tenant: &str) -> Result<bool> {
        let iam = self.iam.lock().unwrap_or_else(|p| p.into_inner());
        match iam.as_ref() {
            Some(iam) => iam.requires_explicit_write_scope(tenant),
            None => Ok(false),
        }
    }

    /// This tenant's cache slot (created empty on first sight). Holds the map lock only for
    /// the lookup.
    fn slot_for(&self, tenant: &str) -> TenantSlot {
        let mut cache = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        cache.entry(tenant.to_string()).or_default().clone()
    }

    /// The cached handle for a tenant, opening (and caching) it on first use. Blocking: call it
    /// on the blocking pool (or a plain thread), never on an async worker. The open runs under
    /// the tenant's OWN slot lock, not the cache-wide one: a first open that migrates a vector
    /// index (a re-embed per scoped record, seconds to minutes on a real embedder) holds up
    /// requests for that tenant only. When the open fails, the slot stays empty and the next
    /// request retries.
    fn memory_for(&self, tenant: &str) -> Result<TenantHandle> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(ShuttingDown.into());
        }
        let slot = self.slot_for(tenant);
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = slot.as_ref() {
            return Ok(h.clone());
        }
        #[cfg(feature = "zvec")]
        let opened = LocalMemory::open_tenant_with_embedder(tenant, self.embedder.clone());
        #[cfg(not(feature = "zvec"))]
        let opened = Memory::open_tenant(tenant);
        let local = match opened {
            Ok(l) => l,
            Err(e) => {
                // DM_REQUIRE_VECTORS refused this tenant: it counts as a tenant without a vector
                // index (readiness), and the open is retried by its next request and, without
                // one, on the retry schedule (`retry_vector_indexes`).
                if e.downcast_ref::<crate::tools::VectorIndexRequired>().is_some() {
                    self.note_vectors(tenant, false);
                }
                return Err(e);
            }
        };
        self.note_vectors(tenant, local.has_vector_index());
        let handle: TenantHandle = Arc::new(Mutex::new(Memory::Local(local)));
        *slot = Some(handle.clone());
        Ok(handle)
    }

    /// Record whether a tenant has a vector index. One without is put on the retry schedule
    /// (a vector build only: a keyword-only build has nothing to retry).
    fn note_vectors(&self, tenant: &str, ok: bool) {
        self.vectors.lock().unwrap_or_else(|p| p.into_inner()).insert(tenant.to_string(), ok);
        let mut retry = self.vector_retry.lock().unwrap_or_else(|p| p.into_inner());
        if ok {
            retry.remove(tenant);
        } else if cfg!(feature = "zvec") {
            let base = self.vector_retry_base;
            retry.entry(tenant.to_string()).or_insert(Retry {
                next: Instant::now() + base,
                delay: base,
                since_ms: crate::entry::now_ms(),
            });
        }
    }

    /// Retry the vector index of every tenant without one whose retry is due, and return how
    /// many were resolved. Without this, a tenant whose index failed to open (a lock another
    /// process held, a file in the way, a full disk) stayed keyword-only, and `/readyz` 503,
    /// until a restart; with `DM_REQUIRE_VECTORS` only that tenant's next request retried, and
    /// a server that is not ready gets no requests. `dmem serve` runs this every second on a
    /// background thread; a failed retry doubles the tenant's wait (5 s up to 5 min).
    #[cfg(feature = "zvec")]
    fn retry_vector_indexes(&self) -> usize {
        let now = Instant::now();
        let mut due: Vec<String> = {
            let retry = self.vector_retry.lock().unwrap_or_else(|p| p.into_inner());
            retry.iter().filter(|(_, r)| r.next <= now).map(|(t, _)| t.clone()).collect()
        };
        due.sort();
        let mut resolved = 0;
        for tenant in due {
            if self.draining.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
                break;
            }
            match self.reopen_for_vectors(&tenant) {
                Ok(Reopened::Resolved) => resolved += 1,
                Ok(Reopened::Healed { old, index, since_ms }) => {
                    resolved += 1;
                    self.reembed_since(&tenant, old, &index, since_ms);
                }
                Ok(Reopened::Not) => self.back_off_vector_retry(&tenant),
                Err(e) => {
                    if e.downcast_ref::<ShuttingDown>().is_none() {
                        eprintln!("dmem serve: retrying tenant '{tenant}' vector index failed ({e:#})");
                    }
                    self.back_off_vector_retry(&tenant);
                }
            }
        }
        resolved
    }

    #[cfg(feature = "zvec")]
    fn back_off_vector_retry(&self, tenant: &str) {
        let mut retry = self.vector_retry.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = retry.get_mut(tenant) {
            r.delay = next_retry_delay(r.delay, self.vector_retry_base);
            r.next = Instant::now() + r.delay;
        }
    }

    /// One retry: open the tenant afresh, under its slot lock (the lock a first open takes; the
    /// tenant's requests wait for it, other tenants do not). When the new handle has a vector
    /// index it replaces the keyword-only one in the cache (requests already holding the old
    /// handle finish on it); otherwise the new handle is dropped and nothing changes.
    #[cfg(feature = "zvec")]
    fn reopen_for_vectors(&self, tenant: &str) -> Result<Reopened> {
        let slot = self.slot_for(tenant);
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        if self.closing.load(Ordering::SeqCst) {
            return Err(ShuttingDown.into());
        }
        if self.tenant_has_vectors(tenant) {
            self.vector_retry.lock().unwrap_or_else(|p| p.into_inner()).remove(tenant);
            return Ok(Reopened::Resolved);
        }
        if slot.is_none() && !tenant_db_exists(tenant) {
            self.vectors.lock().unwrap_or_else(|p| p.into_inner()).remove(tenant);
            self.vector_retry.lock().unwrap_or_else(|p| p.into_inner()).remove(tenant);
            return Ok(Reopened::Resolved);
        }
        let local = match LocalMemory::open_tenant_with_embedder(tenant, self.embedder.clone()) {
            Ok(l) => l,
            // the open already logged why the index is unavailable
            Err(e) if e.downcast_ref::<crate::tools::VectorIndexRequired>().is_some() => return Ok(Reopened::Not),
            Err(e) => return Err(e),
        };
        let Some(index) = local.vector_index() else { return Ok(Reopened::Not) };
        if self.closing.load(Ordering::SeqCst) {
            return Err(ShuttingDown.into());
        }
        let since_ms = self
            .vector_retry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(tenant)
            .map(|r| r.since_ms)
            .unwrap_or_else(crate::entry::now_ms);
        let old = slot.replace(Arc::new(Mutex::new(Memory::Local(local))));
        self.note_vectors(tenant, true);
        eprintln!("dmem serve: tenant '{tenant}' vector index opened on retry; hybrid recall is back");
        Ok(Reopened::Healed { old, index, since_ms })
    }

    /// After a heal: give every uri written while the tenant had no vector index its vectors
    /// again, so hybrid recall (which gates on cosine) finds those records. Waits first for the
    /// requests still on the replaced keyword-only handle (up to `HEAL_DRAIN`), then works on
    /// its own connection without the tenant lock, one uri per save permit, so the tenant's
    /// requests and other saves go on meanwhile. A save of the same uri racing it can leave that
    /// record's vector one version behind (the record itself is always read from the database).
    /// Records saved keyword-only before this process started are not covered: `dmem
    /// reindex-embeddings`, with the server stopped, re-embeds everything.
    #[cfg(feature = "zvec")]
    fn reembed_since(&self, tenant: &str, old: Option<TenantHandle>, index: &crate::zvec_index::ZvecIndex, since_ms: i64) {
        if let Some(old) = old {
            let end = Instant::now() + HEAL_DRAIN;
            while Arc::strong_count(&old) > 1 && Instant::now() < end && !self.draining.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let store = match crate::config::db_path(tenant).and_then(|p| crate::sqlite::SqliteStore::open(&p)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("dmem serve: tenant '{tenant}': cannot re-embed the records saved without a vector index ({e:#})");
                return;
            }
        };
        // a second of slack: `since_ms` and the rows' times come from the same clock, in ms
        let uris = match store.uris_changed_since(since_ms - 1000) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("dmem serve: tenant '{tenant}': cannot list the records saved without a vector index ({e:#})");
                return;
            }
        };
        let (mut vectors, mut failed) = (0usize, 0usize);
        for uri in &uris {
            if self.draining.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
                eprintln!("dmem serve: tenant '{tenant}': stopping; re-embedding after the heal is incomplete");
                return;
            }
            let (_permit, waited) = self.embed_gate.saves.acquire();
            crate::metrics::embed_permit_wait(crate::metrics::EmbedPool::Save, waited);
            match crate::tools::resync_uri_vectors(&store, index, &*self.embedder, uri) {
                Ok(n) => vectors += n,
                Err(e) => {
                    failed += 1;
                    crate::metrics::zvec_failure(crate::metrics::ZvecOp::Upsert);
                    eprintln!("dmem serve: tenant '{tenant}': re-embedding {uri} failed ({e:#})");
                }
            }
        }
        if !uris.is_empty() {
            eprintln!(
                "dmem serve: tenant '{tenant}': re-embedded {vectors} record(s) of {} uri(s) written without a vector index{}",
                uris.len(),
                if failed > 0 { format!(" ({failed} failed)") } else { String::new() }
            );
        }
    }

    /// Every tenant a token can reach: the env tokens' tenants and IAM's active tenants.
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    fn known_tenants(&self) -> Vec<String> {
        let mut out = self.auth.tenants();
        let iam = self.iam.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(iam) = iam.as_ref() {
            match iam.list() {
                Ok(rows) => out.extend(rows.into_iter().filter(|r| r.status == "active").map(|r| r.tenant)),
                Err(e) => eprintln!("dmem serve: could not list IAM tenants for the start-up checks ({e:#})"),
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Start-up: open every known tenant that has a database (tenants never written to have
    /// nothing to judge, and opening them would create files), so `/readyz` judges each
    /// tenant's vector index before the server takes traffic instead of at its first request.
    /// Returns how many it opened.
    #[cfg(feature = "zvec")]
    fn open_known_tenants(&self) -> usize {
        let mut opened = 0;
        for tenant in self.known_tenants() {
            if self.draining.load(Ordering::SeqCst) {
                break;
            }
            let judged = self.vectors.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&tenant);
            if judged || !tenant_db_exists(&tenant) {
                continue;
            }
            match self.memory_for(&tenant) {
                Ok(_) => opened += 1,
                Err(e) => eprintln!("dmem serve: tenant '{tenant}' could not be opened at start-up ({e:#})"),
            }
        }
        opened
    }

    /// Does this (opened) tenant have a vector index? Unknown (not opened yet) reads as no.
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    fn tenant_has_vectors(&self, tenant: &str) -> bool {
        self.vectors.lock().unwrap_or_else(|p| p.into_inner()).get(tenant).copied().unwrap_or(false)
    }

    /// Embed a request's texts BEFORE it takes the tenant lock (P0-5): the model is the slow
    /// part of a save (a long body is up to 64 windows; one took 93.6 s on a small server),
    /// and a request holding the lock through it stalled every other request on the tenant.
    /// Skipped for a tenant without a vector index (nothing would use the vectors), and the
    /// record bodies are skipped for a request that cannot write. Each part waits for a permit
    /// of its own pool first (`EmbedGate`), and gives it back before the tenant lock.
    #[cfg(feature = "zvec")]
    fn pre_embed(&self, tenant: &str, mut plan: EmbedPlan, can_write: bool) -> crate::tools::PreEmbedded {
        use crate::metrics::{embed_permit_wait, EmbedPool};
        use crate::tools::PreEmbedded;
        if !can_write {
            plan.docs.clear();
        }
        let mut pre = PreEmbedded::default();
        if plan.is_empty() || !self.tenant_has_vectors(tenant) {
            return pre;
        }
        if !plan.queries.is_empty() {
            let queries = EmbedPlan { docs: Vec::new(), queries: std::mem::take(&mut plan.queries) };
            let (_permit, waited) = self.embed_gate.queries.acquire();
            embed_permit_wait(EmbedPool::Query, waited);
            pre.merge(PreEmbedded::compute(&*self.embedder, &queries));
        }
        if !plan.docs.is_empty() {
            let (_permit, waited) = self.embed_gate.saves.acquire();
            embed_permit_wait(EmbedPool::Save, waited);
            pre.merge(PreEmbedded::compute(&*self.embedder, &plan));
        }
        pre
    }

    /// Shutdown, after the drain: flush every cached tenant's vector index and close it (its
    /// SQLite connection and index close when the last reference goes). From here on no request
    /// may open a tenant or take a tenant lock (`closing`). A tenant still in its first open
    /// (the start-up slot migration) is left alone: the migration is checkpointed and resumes on
    /// the next start. A tenant whose lock a request still holds is waited for until `deadline`,
    /// then skipped.
    fn close_tenants(&self, deadline: Instant) -> CloseReport {
        use std::sync::TryLockError;
        self.closing.store(true, Ordering::SeqCst);
        let mut report = CloseReport::default();
        let slots: Vec<(String, TenantSlot)> = {
            let cache = self.mem.lock().unwrap_or_else(|p| p.into_inner());
            let mut v: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        for (tenant, slot) in slots {
            let handle = match slot.try_lock() {
                Ok(mut g) => g.take(),
                Err(TryLockError::Poisoned(p)) => p.into_inner().take(),
                Err(TryLockError::WouldBlock) => {
                    report.opening.push(tenant);
                    continue;
                }
            };
            let Some(handle) = handle else { continue };
            let guard = loop {
                match handle.try_lock() {
                    Ok(g) => break Some(g),
                    Err(TryLockError::Poisoned(p)) => break Some(p.into_inner()),
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    Err(TryLockError::WouldBlock) => break None,
                }
            };
            let Some(guard) = guard else {
                report.busy.push(tenant);
                continue;
            };
            match local_ref(&guard).flush_vectors() {
                Ok(true) => report.flushed += 1,
                Ok(false) => {}
                Err(e) => {
                    crate::metrics::zvec_failure(crate::metrics::ZvecOp::Flush);
                    report.failed.push((tenant.clone(), format!("{e:#}")));
                }
            }
            report.closed += 1;
        }
        report
    }

    /// The readiness checks that touch the disk: the IAM database (open, still at its path,
    /// readable) and a probe file in the data dir. Reused for `io_check_ttl`; the caller that
    /// finds them stale refreshes them while holding the cache lock, so concurrent probes wait
    /// for that one result instead of each writing a probe file.
    fn io_checks(&self) -> IoChecks {
        let mut cache = self.io_checks.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache.as_ref().filter(|c| c.at.elapsed() < self.io_check_ttl) {
            return c.clone();
        }
        let mut detail = Vec::new();
        let iam = {
            let iam = self.iam.lock().unwrap_or_else(|p| p.into_inner());
            match iam.as_ref() {
                None => Err("IAM database not open (serving env tokens only)".to_string()),
                Some(i) => i.healthy().map(|_| "ok".to_string()).map_err(|e| {
                    detail.push(format!("iam: {e:#}"));
                    "IAM database unreadable".to_string()
                }),
            }
        };
        let data_dir = probe_data_dir().map(|_| "writable".to_string()).map_err(|e| {
            detail.push(format!("data dir: {e:#}"));
            "data dir not writable".to_string()
        });
        let fresh = IoChecks { at: Instant::now(), iam, data_dir, detail };
        *cache = Some(fresh.clone());
        fresh
    }

    /// The readiness checks behind `GET /readyz` (blocking: the disk checks, cached briefly).
    /// Not ready when: IAM is not open (env-only serving) or its database file is gone or
    /// unreadable; the embedder is the placeholder ("degraded: placeholder embedder": the model
    /// did not load, and `DM_REQUIRE_MODEL=0` let the server start); a known tenant
    /// has no vector index; the data dir cannot be written; the start-up pass has not finished;
    /// or a stop signal arrived. The start-up pass opens every known tenant with a database,
    /// so each is judged before the server takes traffic; a tenant without a vector index is
    /// retried on a schedule, and readiness returns when its index opens.
    fn readiness(&self) -> Readiness {
        let mut checks: Vec<(&'static str, Check)> = Vec::new();
        let io = self.io_checks();
        #[cfg_attr(not(feature = "zvec"), allow(unused_mut))]
        let mut detail = io.detail;
        checks.push(("iam", io.iam));
        #[cfg(feature = "zvec")]
        {
            let name = self.embedder.name();
            checks.push((
                "embedder",
                if name == "hash" {
                    // only under DM_REQUIRE_MODEL=0: otherwise the server does not start on it
                    Err("degraded: placeholder embedder (the placeholder hash embedder is active: the embedding model \
                         did not load, and DM_REQUIRE_MODEL=0 allows it)"
                        .to_string())
                } else {
                    Ok(name.to_string())
                },
            ));
            let (with, mut without) = {
                let v = self.vectors.lock().unwrap_or_else(|p| p.into_inner());
                let without: Vec<String> = v.iter().filter(|(_, ok)| !**ok).map(|(t, _)| t.clone()).collect();
                (v.len() - without.len(), without)
            };
            without.sort();
            checks.push((
                "vectors",
                if without.is_empty() {
                    Ok(format!("{with} open tenant(s), each with a vector index"))
                } else {
                    detail.push(format!("tenants without a vector index: {}", without.join(", ")));
                    Err(format!("{} tenant(s) without a vector index", without.len()))
                },
            ));
        }
        #[cfg(not(feature = "zvec"))]
        {
            checks.push(("embedder", Ok("none (keyword-only build)".to_string())));
            checks.push(("vectors", Ok("none (keyword-only build)".to_string())));
        }
        checks.push(("data_dir", io.data_dir));
        checks.push((
            "startup",
            if self.started.load(Ordering::SeqCst) {
                Ok("done".to_string())
            } else {
                Err("start-up checks still running (opening known tenants)".to_string())
            },
        ));
        checks.push((
            "shutdown",
            if self.draining.load(Ordering::SeqCst) { Err("shutting down".to_string()) } else { Ok("ok".to_string()) },
        ));
        Readiness { checks, detail }
    }

    /// Log a readiness change once (a probe polls every few seconds).
    fn note_readiness(&self, r: &Readiness) {
        let now = if r.ready() {
            None
        } else {
            let mut parts: Vec<String> =
                r.checks.iter().filter_map(|(n, c)| c.as_ref().err().map(|why| format!("{n}: {why}"))).collect();
            parts.extend(r.detail.iter().cloned());
            Some(parts.join("; "))
        };
        let mut last = self.readiness_note.lock().unwrap_or_else(|p| p.into_inner());
        if *last != now {
            match &now {
                Some(why) => eprintln!("dmem serve: not ready: {why}"),
                None if last.is_some() => eprintln!("dmem serve: ready"),
                None => {}
            }
            *last = now;
        }
    }

    /// The `/metrics` exposition (blocking: readiness runs its checks).
    fn metrics_text(&self) -> String {
        let ready = self.readiness();
        self.note_readiness(&ready);
        let tenants_open = {
            let cache = self.mem.lock().unwrap_or_else(|p| p.into_inner());
            // a slot whose lock is held is mid-open: not open yet
            cache.values().filter(|slot| slot.try_lock().map(|g| g.is_some()).unwrap_or(false)).count()
        };
        let tenants_without_vectors =
            self.vectors.lock().unwrap_or_else(|p| p.into_inner()).values().filter(|ok| !**ok).count();
        #[cfg(feature = "zvec")]
        let (embedder, embed_permits) =
            (Some(self.embedder.name()), Some((self.embed_gate.saves.total, self.embed_gate.queries.total)));
        #[cfg(not(feature = "zvec"))]
        let (embedder, embed_permits) = (None, None);
        crate::metrics::render(&crate::metrics::Gauges {
            ready: ready.ready(),
            shutting_down: self.draining.load(Ordering::SeqCst),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            tenants_open,
            tenants_without_vectors,
            embedder,
            embed_permits,
        })
    }
}

/// Can the server still write its data dir (and the tenant databases' directory)? Creates,
/// writes, syncs and removes a probe file in each. A read-only remount or a full disk fails it.
fn probe_data_dir() -> Result<()> {
    let data = crate::config::data_dir()?;
    let mut dirs = vec![data.clone()];
    let tenants = data.join("tenants");
    if tenants.is_dir() {
        dirs.push(tenants);
    }
    for d in dirs {
        probe_dir(&d).map_err(|e| anyhow::anyhow!("{}: {e}", d.display()))?;
    }
    Ok(())
}

/// Numbers the readiness probe files, so each probe has a name of its own.
static PROBE_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Create a new probe file in `dir` (never an existing path, never through a symlink), write
/// and sync it, then remove it.
fn probe_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    for _ in 0..3 {
        let probe = dir.join(format!(".readyz-probe-{}-{}", std::process::id(), PROBE_SEQ.fetch_add(1, Ordering::Relaxed)));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = match opts.open(&probe) {
            Ok(f) => f,
            // a name already taken (a stale probe, or something planted there): not ours to
            // touch; take the next name
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let r = f.write_all(b"ok").and_then(|_| f.sync_all());
        drop(f);
        let _ = std::fs::remove_file(&probe);
        return r;
    }
    Err(std::io::Error::other("no free probe file name"))
}

/// Does `tenant` have a database file? (Without creating the tenants dir, as `db_path` would.)
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
fn tenant_db_exists(tenant: &str) -> bool {
    crate::config::data_dir()
        .map(|d| d.join("tenants").join(format!("{}.db", crate::config::canonical_tenant(tenant))).exists())
        .unwrap_or(false)
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

/// An engine guard's refusal (`tools::Refusal`): 400 carrying its message, on every route. The
/// message names the rule that refused and nothing internal, so the caller can act on it; any
/// other failure keeps the route's generic body.
fn refusal(e: &anyhow::Error) -> Option<ApiResp> {
    let r = e.downcast_ref::<crate::tools::Refusal>()?;
    eprintln!("dmem serve: refused: {r}");
    Some(err(StatusCode::BAD_REQUEST, r.message()))
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
            Err(e) => refusal(&e).unwrap_or_else(|| internal(e)),
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
    /// namespace prefixes to restrict the hit set to (knowledge plane file scope)
    #[serde(default)]
    namespaces: Option<Vec<String>>,
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

/// The reply to a successful save: its uri, and the scope it was stamped with. The scope is
/// additive (older clients read only `uri`); a client that named a write scope checks it, so a
/// write that did not land where it asked is never reported as done.
fn saved(m: &LocalMemory, uri: String) -> serde_json::Value {
    json!({ "uri": uri, "scope": m.stamped_scope() })
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
/// in parallel. The embedding model does NOT run under that Mutex: a route that embeds names
/// its texts up front (`with_tenant_plan`) and they are embedded before the lock is taken.
/// (Admin tokens have no tenant -> 401 here.)
/// A request's scope binding: the write scope stamped on its writes (None = writes refused),
/// the read set (None = full tenant), and, when writes are refused for a reason other than a
/// read-only token, that reason (the tenant requires an explicit write scope the request did
/// not name; an adapter named a write scope outside its token's cap).
#[derive(Debug, PartialEq)]
struct ScopeBinding {
    write: Option<String>,
    read: Option<Vec<String>>,
    write_refusal: Option<String>,
}

/// The answer to an `X-DM-Write-Scope` header the server cannot use as a scope.
const INVALID_WRITE_SCOPE: &str =
    "X-DM-Write-Scope must be one scope of printable ASCII text without commas ('' = global)";

/// The `X-DM-Write-Scope` header, as sent.
#[derive(Debug, PartialEq)]
enum WriteScopeHeader {
    Absent,
    /// The scope, trimmed; "" = a deliberate global write.
    Named(String),
    /// Present but not a usable scope: bytes outside printable ASCII, a comma, or the header
    /// repeated. Never read as "absent": that would land the write in the token's default
    /// scope (global, for a full-tenant token), which is exactly what naming a scope avoids.
    Invalid,
}

fn write_scope_header(headers: &HeaderMap) -> WriteScopeHeader {
    let mut values = headers.get_all("x-dm-write-scope").iter();
    let Some(v) = values.next() else { return WriteScopeHeader::Absent };
    if values.next().is_some() {
        return WriteScopeHeader::Invalid;
    }
    match v.to_str().map(str::trim) {
        Ok(s) if crate::entry::valid_write_scope(s) => WriteScopeHeader::Named(s.to_string()),
        _ => WriteScopeHeader::Invalid,
    }
}

/// Per-request scope binding derived from the token (+ scope headers).
///
/// Scope-unbound tokens (every pre-scope token) read the full tenant and, with no
/// `X-DM-Write-Scope`, write global - byte-identical to the pre-scope server - unless the
/// tenant requires an explicit write scope (`require_explicit_write_scope`), in which case a
/// request without the header cannot write at all. With the header, the token's writes are
/// stamped with the named scope ('' = a deliberate global write) on any tenant; naming a scope
/// only narrows what a full-tenant token could already write, and it is what `dmem --scope`
/// sends. Reads are unchanged either way (`X-DM-Scopes` is ignored for these tokens).
/// Non-adapter scoped tokens asserting scope headers are silently ignored (Q1): their grants
/// are static.
fn scope_binding(id: &crate::iam::Identity, headers: &HeaderMap, require_explicit_write_scope: bool) -> ScopeBinding {
    let header = write_scope_header(headers);
    if header == WriteScopeHeader::Invalid {
        // `on_tenant` answers 400 before binding; fail closed here all the same.
        let read = if id.scope_unbound() { None } else { restricted_scope_binding(id, &WriteScopeHeader::Absent, headers).0 .1 };
        return ScopeBinding { write: None, read, write_refusal: Some(INVALID_WRITE_SCOPE.into()) };
    }
    if id.scope_unbound() {
        return match header {
            WriteScopeHeader::Named(w) => ScopeBinding { write: Some(w), read: None, write_refusal: None },
            _ if require_explicit_write_scope => ScopeBinding {
                write: None,
                read: None,
                write_refusal: Some(crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED.into()),
            },
            _ => ScopeBinding { write: Some(String::new()), read: None, write_refusal: None },
        };
    }
    let ((write, read), write_refusal) = restricted_scope_binding(id, &header, headers);
    ScopeBinding { write, read, write_refusal }
}

/// The (write, read) binding of a scope-bound token: an adapter's per-request assertion, or a
/// plain scoped token's static grants; plus the reason writes are refused when an adapter
/// asserted a write scope its token may not use.
#[allow(clippy::type_complexity)]
fn restricted_scope_binding(
    id: &crate::iam::Identity,
    header: &WriteScopeHeader,
    headers: &HeaderMap,
) -> ((Option<String>, Option<Vec<String>>), Option<String>) {
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
        let write = match header {
            WriteScopeHeader::Named(w) => Some(w.clone()),
            _ => None,
        };
        // A capped adapter's cap bounds its writes too: the write scope joins the read set
        // below, so an uncapped write scope would let it read (and write) any scope just by
        // naming it. A scope outside the cap, or global (promotion is an uncapped adapter's
        // job), leaves this request read-only, with the reason for its writes.
        if let (Some(cap), Some(w)) = (&id.scope_read, write.as_deref()) {
            let refusal = if w.is_empty() {
                Some("this adapter token's scope cap does not cover global writes".to_string())
            } else if !cap.iter().any(|c| c == w) {
                Some(format!("write scope '{w}' is outside this adapter token's scope cap"))
            } else {
                None
            };
            if refusal.is_some() {
                return ((None, Some(read)), refusal);
            }
        }
        if let Some(w) = write.as_deref().filter(|w| !w.is_empty()) {
            if !read.iter().any(|s| s == w) {
                read.push(w.to_string());
            }
        }
        return ((write, Some(read)), None);
    }
    // Plain scope-bound token: static grants. Missing scope_read = global-only (fail closed);
    // the write scope is always readable so engine-side mutation guards can see their target.
    let mut read = id.scope_read.clone().unwrap_or_default();
    if let Some(w) = id.scope_write.as_deref().filter(|w| !w.is_empty()) {
        if !read.iter().any(|s| s == w) {
            read.push(w.to_string());
        }
    }
    ((id.scope_write.clone(), Some(read)), None)
}

async fn with_tenant<F>(st: &AppState, headers: &HeaderMap, client_err: bool, f: F) -> ApiResp
where
    F: FnOnce(&LocalMemory, Option<String>) -> Result<serde_json::Value> + Send + 'static,
{
    with_tenant_plan(st, headers, client_err, EmbedPlan::default, f).await
}

/// As `with_tenant`, for a request that embeds: `plan` names the texts (record bodies to save,
/// recall queries), and they are embedded before the tenant lock is taken (see
/// `AppState::pre_embed`). It runs after authentication, on the blocking pool.
async fn with_tenant_plan<P, F>(st: &AppState, headers: &HeaderMap, client_err: bool, plan: P, f: F) -> ApiResp
where
    P: FnOnce() -> EmbedPlan + Send + 'static,
    F: FnOnce(&LocalMemory, Option<String>) -> Result<serde_json::Value> + Send + 'static,
{
    match on_tenant(st, headers, plan, move |m, agent| f(local_ref(m), agent)).await {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => {
            if let Some(resp) = refusal(&e) {
                resp
            } else if client_err {
                bad_request(e)
            } else {
                internal(e)
            }
        }
        Err(resp) => resp,
    }
}

/// The per-request core shared by `with_tenant` and the MCP route: resolve the bearer to a
/// tenant identity, take the tenant's cached handle, bind this request's scope + agent context
/// under the tenant lock on the blocking pool, run `f`, reset the context. `Err` is an already
/// rendered response (401 for a missing/unknown/admin token, 500 for an unopenable tenant or a
/// failed task); `Ok` carries `f`'s own outcome for the caller to shape.
async fn on_tenant<T, P, F>(st: &AppState, headers: &HeaderMap, plan: P, f: F) -> std::result::Result<Result<T>, ApiResp>
where
    T: Send + 'static,
    P: FnOnce() -> EmbedPlan + Send + 'static,
    F: FnOnce(&Memory, Option<String>) -> Result<T> + Send + 'static,
{
    let id = match resolve_identity(st, headers) {
        Some(id) if id.tenant.is_some() => id,
        _ => return Err(err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token")),
    };
    // A write scope the server cannot use is refused outright, whatever the token: reading
    // it as "no header" would land the request's writes in the token's default scope.
    if write_scope_header(headers) == WriteScopeHeader::Invalid {
        eprintln!("dmem serve: refused: {INVALID_WRITE_SCOPE}");
        return Err(err(StatusCode::BAD_REQUEST, INVALID_WRITE_SCOPE));
    }
    let tenant = id.tenant.clone().expect("checked above");
    let agent = id.agent.clone();
    // The tenant policy only concerns scope-unbound tokens; skip the lookup for the others.
    let unbound = id.scope_unbound();
    let policy = if unbound { st.requires_explicit_write_scope(&tenant).map_err(internal)? } else { false };
    let binding = scope_binding(&id, headers, policy);
    let st = st.clone();
    let out = tokio::task::spawn_blocking(move || -> std::result::Result<Result<T>, anyhow::Error> {
        // Opened (and on first use migrated) here on the blocking pool, never on an async
        // worker, and under this tenant's own slot only: see `memory_for`.
        let handle = st.memory_for(&tenant)?;
        // The model runs here, before the tenant lock: the lock covers only the SQLite work and
        // the vector upsert, so a long save no longer stalls the tenant's other requests.
        #[cfg(feature = "zvec")]
        let pre = st.pre_embed(&tenant, plan(), binding.write.is_some());
        #[cfg(not(feature = "zvec"))]
        let _ = plan;
        if st.closing.load(Ordering::SeqCst) {
            return Err(ShuttingDown.into());
        }
        let waited = Instant::now();
        let mut guard = handle.lock().unwrap_or_else(|p| p.into_inner());
        crate::metrics::lock_wait(waited.elapsed());
        // Again under the lock: a stop may have begun while this request waited for it.
        // `close_tenants` sets `closing` before it takes this same lock to flush, so a request
        // that finds it clear here runs before that flush, and one that finds it set never
        // writes after it.
        if st.closing.load(Ordering::SeqCst) {
            return Err(ShuttingDown.into());
        }
        let held = Instant::now();
        // Bind this request's scope context, run, then RESET: the handle is cached per
        // tenant and the next request must never inherit a previous caller's context.
        let local = local_mut(&mut guard);
        local.set_scope_context(binding.write.as_deref(), binding.read);
        local.set_write_refusal(binding.write_refusal);
        local.set_scope_unbound(unbound);
        local.set_agent_view(agent.clone());
        #[cfg(feature = "zvec")]
        local.set_pre_embedded(pre);
        let r = f(&guard, agent);
        let local = local_mut(&mut guard);
        local.set_scope_context(Some(""), None);
        local.set_write_refusal(None);
        local.set_scope_unbound(false);
        local.set_agent_view(None);
        #[cfg(feature = "zvec")]
        local.clear_pre_embedded();
        drop(guard);
        crate::metrics::lock_hold(held.elapsed());
        Ok(r)
    })
    .await
    .map_err(|e| internal(anyhow::anyhow!("memory task failed: {e}")))?;
    out.map_err(|e| {
        if e.downcast_ref::<ShuttingDown>().is_some() {
            err(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down")
        } else {
            internal(e)
        }
    })
}

async fn healthz() -> ApiResp {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// `GET /readyz` (no token: it is what an orchestrator probes): 200 when this server should
/// take traffic, 503 with the failing checks otherwise (see `AppState::readiness`). Without a
/// token the body is each check's name and `pass` or `fail`, nothing more; the admin token or
/// `DM_METRICS_TOKEN` also gets each check's detail (the embedder's name, how many tenants,
/// why IAM is out). No body names a tenant, path or the version; the server log has those.
/// `/healthz` stays a plain liveness answer.
async fn readyz_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    let (r, detailed) = match tokio::task::spawn_blocking(move || {
        let detailed = headers.contains_key(AUTHORIZATION) && metrics_access(&st, &headers).is_ok();
        let r = st.readiness();
        st.note_readiness(&r);
        (r, detailed)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(anyhow::anyhow!("readiness task failed: {e}")),
    };
    let checks: serde_json::Map<String, Value> = r
        .checks
        .iter()
        .map(|(n, c)| {
            let shown = match (detailed, c) {
                (true, Ok(v) | Err(v)) => v.as_str(),
                (false, Ok(_)) => "pass",
                (false, Err(_)) => "fail",
            };
            (n.to_string(), json!(shown))
        })
        .collect();
    if r.ready() {
        (StatusCode::OK, Json(json!({ "status": "ready", "checks": checks })))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "not ready", "failing": r.failing(), "checks": checks })))
    }
}

/// May this request read the metrics (and the readiness details)? The admin token or
/// `DM_METRICS_TOKEN` may; the refusal is 403 for any other valid token, 401 for none.
/// Blocking (the IAM lookup).
fn metrics_access(st: &AppState, headers: &HeaderMap) -> std::result::Result<(), ApiResp> {
    let presented = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(parse_bearer);
    if presented.is_some_and(|t| st.metrics_token.as_deref() == Some(sha256_hex(t).as_str())) {
        return Ok(());
    }
    match resolve_identity(st, headers) {
        Some(id) if id.is_admin => Ok(()),
        Some(_) => Err(err(StatusCode::FORBIDDEN, "admin or metrics token required")),
        None => Err(err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token")),
    }
}

/// `GET /metrics`: Prometheus text exposition (lock wait and hold, request latency per route,
/// embedding time before and under the tenant lock, the active embedder, vector-index
/// failures, readiness). Aggregate only, no tenant names. Readable with the admin token or
/// `DM_METRICS_TOKEN`; a tenant token gets 403.
async fn metrics_h(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(refused) = metrics_access(&st, &headers) {
        return refused.into_response();
    }
    match tokio::task::spawn_blocking(move || st.metrics_text()).await {
        Ok(text) => ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response(),
        Err(e) => internal(anyhow::anyhow!("metrics task failed: {e}")).into_response(),
    }
}

/// `GET /capabilities` (any tenant token): what this server supports, so a client can check
/// before it relies on a feature an older server would silently ignore. Servers before 0.3.7
/// answer 404 here. `write_scope`: `X-DM-Write-Scope` is understood; `write_scope_header`:
/// THIS token's writes follow it (a scope-unbound or adapter token), where a plain scoped
/// token always writes to its own scope. Authenticated, so the version is not advertised to
/// anyone who can reach the port.
async fn capabilities_h(State(st): State<AppState>, headers: HeaderMap) -> ApiResp {
    match resolve_identity(&st, &headers) {
        Some(id) if id.tenant.is_some() => (
            StatusCode::OK,
            Json(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "write_scope": true,
                "write_scope_header": id.scope_unbound() || id.adapter,
            })),
        ),
        _ => err(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"),
    }
}

async fn recall_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RecallReq>) -> ApiResp {
    // an as-of recall is keyword-only: nothing to embed
    let plan = if req.as_of.is_none() { EmbedPlan::query(req.query.clone()) } else { EmbedPlan::default() };
    with_tenant_plan(&st, &headers, false, move || plan, move |m, _agent| {
        let limit = req.limit.unwrap_or(6).min(MAX_LIMIT);
        let prefixes = req.namespaces.clone().unwrap_or_default();
        let hits = match req.as_of {
            Some(ts) => m.recall_as_of(&req.query, limit, ts, req.valid.unwrap_or(ts))?,
            None if prefixes.is_empty() => m.recall(&req.query, limit)?,
            None => m.recall_in(&req.query, limit, &prefixes)?,
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
    let plan = EmbedPlan::doc(req.text.clone());
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        let uri = m.remember(&req.text, ns_or(&req.namespace, "resources/notes"), req.valid_from, req.valid_to, agent.as_deref())?;
        Ok(saved(m, uri))
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
    let plan = EmbedPlan::query(req.query.clone());
    with_tenant_plan(&st, &headers, false, move || plan, move |m, _agent| {
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
    let plan = EmbedPlan::doc(crate::tools::decision_body(&req.title, &req.context, &req.decision, &req.rationale));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        let ns = ns_or(&req.namespace, "resources/notes");
        Ok(saved(m, m.log_decision(&req.title, &req.context, &req.decision, &req.rationale, ns, agent.as_deref())?))
    })
    .await
}

async fn lesson_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<LessonReq>) -> ApiResp {
    let plan = EmbedPlan::doc(crate::tools::lesson_body(&req.title, &req.lesson));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        Ok(saved(m, m.log_lesson(&req.title, &req.lesson, ns_or(&req.namespace, "agent/lessons"), agent.as_deref())?))
    })
    .await
}

async fn incident_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<IncidentReq>) -> ApiResp {
    let plan = EmbedPlan::doc(crate::tools::incident_body(&req.title, &req.impact, &req.resolution));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        let ns = ns_or(&req.namespace, "resources/incidents");
        Ok(saved(m, m.log_incident(&req.title, &req.impact, &req.resolution, ns, agent.as_deref())?))
    })
    .await
}

async fn runbook_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<RunbookReq>) -> ApiResp {
    let plan = EmbedPlan::doc(crate::tools::runbook_body(&req.title, &req.steps));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        Ok(saved(m, m.log_runbook(&req.title, &req.steps, ns_or(&req.namespace, "resources/runbooks"), agent.as_deref())?))
    })
    .await
}

async fn convention_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ConventionReq>) -> ApiResp {
    let plan = EmbedPlan::doc(crate::tools::convention_body(&req.title, &req.rule));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        Ok(saved(m, m.log_convention(&req.title, &req.rule, ns_or(&req.namespace, "resources/conventions"), agent.as_deref())?))
    })
    .await
}

async fn reminder_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<ReminderReq>) -> ApiResp {
    let plan = EmbedPlan::doc(crate::tools::reminder_body(&req.title, &req.text));
    with_tenant_plan(&st, &headers, true, move || plan, move |m, agent| {
        Ok(saved(m, m.add_reminder(&req.title, &req.text, ns_or(&req.namespace, "agent/reminders"), agent.as_deref())?))
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
    let plan = EmbedPlan::doc(req.body.clone());
    with_tenant_plan(&st, &headers, true, move || plan, move |m, _agent| {
        let kind = crate::entry::Kind::from_str(&req.kind)
            .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", req.kind))?;
        let ns = if req.namespace.is_empty() { "resources/notes" } else { &req.namespace };
        let uri = if req.created_ms > 0 || req.importance.is_some() {
            m.import_record_at(kind, ns, &req.title, &req.body, req.created_ms, req.importance)?
        } else {
            m.import_record(kind, ns, &req.title, &req.body)?
        };
        Ok(saved(m, uri))
    })
    .await
}

/// `POST /mcp`: the MCP streamable-HTTP transport (protocol 2025-03-26), stateless. One JSON-RPC
/// request or notification per call, dispatched through the same `mcp::handle` as stdio so the
/// tool surface cannot drift between transports. A request gets its JSON-RPC reply as
/// `application/json`; a notification (no `id`) gets 202 with an empty body. No session id: the
/// bearer is the session. The body is taken raw (not the Json extractor) so that auth runs
/// BEFORE any parsing, and a malformed body answers as JSON-RPC (400 + parse error) rather than
/// the extractor's rejection.
async fn mcp_h(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // A body that never becomes a JSON-RPC request: 400 carrying the null-id error object.
    fn rejected(code: i64, message: &str) -> (StatusCode, Option<Value>) {
        (StatusCode::BAD_REQUEST, Some(crate::mcp::protocol_error(code, message)))
    }
    // The texts a tool call will embed, read from the body after authentication (on_tenant
    // runs the plan after it has resolved the token).
    let plan_body = body.clone();
    let plan = move || serde_json::from_slice::<Value>(&plan_body).map(|v| crate::mcp::embed_plan(&v)).unwrap_or_default();
    let out = on_tenant(&st, &headers, plan, move |m, agent| {
        let req = match serde_json::from_slice::<Value>(&body) {
            Ok(v) if v.is_object() => v,
            Ok(_) => return Ok(rejected(crate::mcp::INVALID_REQUEST, "invalid request")),
            Err(_) => return Ok(rejected(crate::mcp::PARSE_ERROR, "parse error")),
        };
        // `instructions` (persona + protocols) rides only on `initialize`, as on stdio. Rendered
        // per call from the token's agent view (one server serves many tenants and agents), not
        // once per process.
        let instructions = if req.get("method").and_then(|v| v.as_str()) == Some("initialize") {
            local_ref(m)
                .persona_for(agent.as_deref())
                .ok()
                .filter(|p| !p.is_empty())
                .map(|p| crate::render::render_instructions(&p))
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        Ok(match crate::mcp::handle(m, &instructions, &req) {
            Some(resp) => (StatusCode::OK, Some(resp)),
            None => (StatusCode::ACCEPTED, None),
        })
    })
    .await;
    match out {
        Ok(Ok((status, Some(v)))) => (status, Json(v)).into_response(),
        Ok(Ok((status, None))) => status.into_response(),
        Ok(Err(e)) => internal(e).into_response(),
        Err(resp) => resp.into_response(),
    }
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
            .map(|r| {
                json!({
                    "tenant": r.tenant,
                    "status": r.status,
                    "tokens": r.tokens,
                    "agents": r.agents,
                    "require_explicit_write_scope": r.require_explicit_write_scope,
                })
            })
            .collect();
        Ok(json!(rows))
    })
}

#[derive(Deserialize)]
struct AdminPolicyReq {
    tenant: String,
    /// Absent = report the current policy without changing it.
    #[serde(default)]
    require_explicit_write_scope: Option<bool>,
}

/// Read or set a tenant's write-scope policy: with `require_explicit_write_scope` on, every
/// write by a full-tenant token must name its scope in `X-DM-Write-Scope` ('' = global).
async fn admin_policy_h(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<AdminPolicyReq>) -> ApiResp {
    with_admin(&st, &headers, || {
        let iam = crate::iam::Iam::open()?;
        if let Some(on) = req.require_explicit_write_scope {
            iam.set_requires_explicit_write_scope(&req.tenant, on)?;
        }
        let tenant = crate::config::canonical_tenant(&req.tenant);
        if !iam.list()?.iter().any(|r| r.tenant == tenant) {
            return Err(crate::tools::Refusal::new(format!("unknown tenant '{tenant}'")).into());
        }
        Ok(json!({ "tenant": tenant, "require_explicit_write_scope": iam.requires_explicit_write_scope(&tenant)? }))
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
/// the per-tenant memory cache starts empty and fills on first use. `/mcp` is POST-only: a GET
/// answers 405 from the router itself (no server-initiated messages, so no event stream).
pub fn router(auth: Arc<dyn Authenticator>, iam: Option<crate::iam::Iam>) -> Router {
    router_with_state(app_state(auth, iam))
}

fn app_state(auth: Arc<dyn Authenticator>, iam: Option<crate::iam::Iam>) -> AppState {
    AppState {
        auth,
        iam: Arc::new(Mutex::new(iam)),
        mem: Arc::new(Mutex::new(HashMap::new())),
        vectors: Arc::new(Mutex::new(HashMap::new())),
        vector_retry: Arc::new(Mutex::new(HashMap::new())),
        vector_retry_base: VECTOR_RETRY_BASE,
        // A router without `run_blocking`'s start-up pass has nothing pending.
        started: Arc::new(AtomicBool::new(true)),
        io_checks: Arc::new(Mutex::new(None)),
        io_check_ttl: IO_CHECK_TTL,
        #[cfg(feature = "zvec")]
        embed_gate: Arc::new(EmbedGate {
            saves: Permits::new(crate::config::embed_concurrency()),
            queries: Permits::new(crate::config::embed_query_concurrency()),
        }),
        #[cfg(feature = "zvec")]
        embedder: crate::tools::shared_embedder(),
        in_flight: Arc::new(AtomicUsize::new(0)),
        draining: Arc::new(AtomicBool::new(false)),
        closing: Arc::new(AtomicBool::new(false)),
        metrics_token: metrics_token_from_env().ok().flatten().map(|t| sha256_hex(&t)),
        readiness_note: Arc::new(Mutex::new(None)),
    }
}

/// `DM_METRICS_TOKEN`: a scrape-only bearer for `/metrics` (unset or empty = only the admin
/// token may read it). At least 16 characters, like the `DM_TOKEN_*` secrets.
fn metrics_token_from_env() -> Result<Option<String>> {
    match std::env::var("DM_METRICS_TOKEN") {
        Ok(t) if t.is_empty() => Ok(None),
        Ok(t) if t.len() < 16 => {
            anyhow::bail!("DM_METRICS_TOKEN is too short ({} chars): bearer secrets must be at least 16 characters", t.len())
        }
        Ok(t) => Ok(Some(t)),
        Err(_) => Ok(None),
    }
}

/// Counts a request in `AppState::in_flight` for as long as it is handled (dropped with the
/// request, so a cancelled one is uncounted too).
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(n: &Arc<AtomicUsize>) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        InFlight(n.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn track_requests(State(st): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let _in_flight = InFlight::enter(&st.in_flight);
    // the route pattern, never the raw path (bounded label set)
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let t = Instant::now();
    let resp = next.run(req).await;
    crate::metrics::request(&route, resp.status().as_u16(), t.elapsed());
    resp
}

/// Open, and so migrate, every tenant whose vector index predates per-scope slots, one after
/// the other, caching each handle for the requests that follow. `dmem serve` runs this on a
/// background thread as it starts listening: a request for a tenant under migration waits for
/// it (on that tenant's slot), every other tenant is served at once, and `/healthz` answers
/// throughout, so a health monitor does not restart the service mid-way (and if something
/// does, the migration resumes from its checkpoint).
#[cfg(feature = "zvec")]
fn migrate_pending_vector_indexes(st: &AppState) {
    let pending = match crate::tools::tenants_pending_slot_migration() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dmem serve: could not list tenants for the vector slot migration ({e:#}); each migrates on its first request");
            return;
        }
    };
    for tenant in pending {
        if st.draining.load(Ordering::SeqCst) {
            eprintln!("dmem serve: shutting down; the vector slot migration of '{tenant}' and later tenants runs on the next start");
            return;
        }
        eprintln!("dmem serve: migrating tenant '{tenant}' vector index to per-scope slots (its requests wait; other tenants are served)");
        match st.memory_for(&tenant) {
            Ok(_) => eprintln!("dmem serve: tenant '{tenant}' vector index ready"),
            Err(e) => eprintln!("dmem serve: tenant '{tenant}' could not be opened ({e:#}); retried on its next request"),
        }
    }
}

/// `dmem serve`'s background thread: the start-up pass (the vector slot migration, then every
/// known tenant opened so readiness judges it), then, once a second until a stop, the retries
/// of tenants without a vector index. `/readyz` is not ready until the start-up pass is done.
#[cfg(feature = "zvec")]
fn background(st: &AppState) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    // A panic here is a bug, but it must neither keep the server not ready for good nor end
    // the retries: it is logged, the pass counts as done, and the next tick runs as usual.
    match catch_unwind(AssertUnwindSafe(|| {
        migrate_pending_vector_indexes(st);
        st.open_known_tenants()
    })) {
        Ok(opened) => eprintln!("dmem serve: start-up checks done ({opened} known tenant(s) opened)"),
        Err(_) => eprintln!("dmem serve: the start-up checks failed (see the panic above); serving without them"),
    }
    st.started.store(true, Ordering::SeqCst);
    while !st.draining.load(Ordering::SeqCst) && !st.closing.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        if catch_unwind(AssertUnwindSafe(|| st.retry_vector_indexes())).is_err() {
            eprintln!("dmem serve: a vector index retry failed (see the panic above); retrying on schedule");
        }
    }
}

fn router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz_h))
        .route("/metrics", get(metrics_h))
        .route("/capabilities", get(capabilities_h))
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
        .route("/mcp", post(mcp_h))
        .route("/admin/tenant", post(admin_add_h))
        .route("/admin/tenants", get(admin_list_h))
        .route("/admin/tenant_policy", post(admin_policy_h))
        .route("/admin/revoke", post(admin_revoke_h))
        .route("/admin/rm", post(admin_rm_h))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(state.clone(), track_requests))
        .with_state(state)
}

/// TLS choice for the server: bring-your-own cert/key, or generate a self-signed pair.
pub struct TlsOpts {
    pub cert: Option<String>,
    pub key: Option<String>,
    pub generate: bool,
    /// Extra SAN entries for the generated cert (`--tls-san`): the public names clients use.
    pub sans: Vec<String>,
}

/// SAN list for the generated cert: localhost, the bind host when it is a real name (a
/// `0.0.0.0` bind names nothing a client could connect to), then the operator's `--tls-san`
/// entries, deduplicated in order. Without the public name here, a client that pins the cert
/// via `ca_cert` still fails hostname verification, which is what used to push people to
/// disable verification altogether.
fn self_signed_sans(addr: &str, extra: &[String]) -> Vec<String> {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    let mut sans = vec!["localhost".to_string()];
    if !host.is_empty() && host != "0.0.0.0" && host != "localhost" {
        sans.push(host.to_string());
    }
    for s in extra {
        let s = s.trim();
        if !s.is_empty() && !sans.iter().any(|x| x == s) {
            sans.push(s.to_string());
        }
    }
    sans
}

/// Generate a self-signed cert + key (PEM), persisting them under `<data>/tls/` so clients
/// can pin the cert via `ca_cert`.
fn generate_self_signed(addr: &str, extra_sans: &[String]) -> Result<(String, String)> {
    let sans = self_signed_sans(addr, extra_sans);
    let ck = rcgen::generate_simple_self_signed(sans.clone()).map_err(|e| anyhow::anyhow!("rcgen: {e}"))?;
    let cert_pem = ck.cert.pem();
    let key_pem = ck.key_pair.serialize_pem();
    if let Ok(dir) = crate::config::data_dir() {
        let tdir = dir.join("tls");
        let _ = std::fs::create_dir_all(&tdir);
        let cpath = tdir.join("cert.pem");
        let _ = std::fs::write(&cpath, &cert_pem);
        // The certificate is public (clients copy it to pin it): readable by all, as before the
        // server's 077 umask. The key beside it stays 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&cpath, std::fs::Permissions::from_mode(0o644));
        }
        // the private key is a secret: 0600, unlike the (public) cert beside it
        let _ = crate::config::write_secret(&tdir.join("key.pem"), &key_pem);
        eprintln!("dmem serve: generated self-signed cert at {} (SANs: {})", cpath.display(), sans.join(", "));
        eprintln!("           clients: copy that file and run `dmem login <url> --ca-cert <file>`;");
        eprintln!("           if the name clients use is missing from the SANs, restart with --tls-san <name>.");
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

/// How long a stop waits for in-flight requests (`DM_SHUTDOWN_GRACE_SECS`, default 15 s, at
/// most an hour). After it, a stop can take up to 7 s more (1 s backing up the TLS drain,
/// `CLOSE_BUDGET` for a tenant lock a straggler holds, 1 s for the runtime to let go) plus the
/// flushes, so the service manager's stop timeout must be at least the grace plus 10 s, and an
/// orchestrator's termination grace period must also cover its pre-stop sleep: the default 30 s
/// fits a 5 s pre-stop with the default 15 s grace.
fn shutdown_grace() -> Duration {
    parse_shutdown_grace(std::env::var("DM_SHUTDOWN_GRACE_SECS").ok().as_deref())
}

const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// After the drain: how long closing the tenants waits for a tenant lock a straggling request
/// still holds before it skips that tenant.
const CLOSE_BUDGET: Duration = Duration::from_secs(5);

fn parse_shutdown_grace(v: Option<&str>) -> Duration {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_SHUTDOWN_GRACE,
        Some(s) => match s.parse::<u64>() {
            Ok(n) if n <= 3600 => Duration::from_secs(n),
            _ => {
                eprintln!("dmem serve: DM_SHUTDOWN_GRACE_SECS={s:?} is not 0-3600 seconds; using {}s", DEFAULT_SHUTDOWN_GRACE.as_secs());
                DEFAULT_SHUTDOWN_GRACE
            }
        },
    }
}

/// Resolves when the process is asked to stop: SIGTERM (a service manager, an orchestrator
/// moving the workload) or SIGINT (ctrl-c). Returns the signal's name.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = tokio::signal::ctrl_c() => "SIGINT",
            },
            Err(e) => {
                eprintln!("dmem serve: cannot watch SIGTERM ({e}); only ctrl-c stops the server gracefully");
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

/// Wait for a server future to finish draining, at most `limit`.
async fn drain<F>(server: std::pin::Pin<&mut F>, limit: Duration) -> Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    match tokio::time::timeout(limit, server).await {
        Ok(r) => r.map_err(|e| anyhow::anyhow!("serve: {e}")),
        Err(_) => {
            eprintln!("dmem serve: requests still in flight after {}s; closing their connections", limit.as_secs());
            Ok(())
        }
    }
}

/// Serve plain HTTP until `stop` resolves, then stop accepting, let in-flight requests finish
/// (at most `grace`) and return.
async fn serve_plain<S>(listener: tokio::net::TcpListener, app: Router, stop: S, grace: Duration) -> Result<()>
where
    S: std::future::Future<Output = ()>,
{
    use std::future::IntoFuture;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut rx = rx;
            let _ = rx.wait_for(|stop| *stop).await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        r = &mut server => return r.map_err(|e| anyhow::anyhow!("serve: {e}")),
        _ = stop => {}
    }
    let _ = tx.send(true);
    drain(server, grace).await
}

/// The HTTPS config: the operator's cert/key, or a fresh self-signed pair (`--tls-generate`);
/// None serves plain HTTP. It installs the process TLS provider first (`crate::tls`), so a
/// client that offers the X25519MLKEM768 hybrid gets it and one without ML-KEM a classical group.
async fn tls_config(addr: &str, tls: &TlsOpts) -> Result<Option<axum_server::tls_rustls::RustlsConfig>> {
    crate::tls::install_provider();
    Ok(if let (Some(c), Some(k)) = (&tls.cert, &tls.key) {
        Some(
            axum_server::tls_rustls::RustlsConfig::from_pem_file(c, k)
                .await
                .map_err(|e| anyhow::anyhow!("load TLS cert/key: {e}"))?,
        )
    } else if tls.generate {
        let (cert_pem, key_pem) = generate_self_signed(addr, &tls.sans)?;
        Some(
            axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("self-signed TLS: {e}"))?,
        )
    } else {
        None
    })
}

/// As `serve_plain`, over TLS.
async fn serve_tls<S>(
    sock: std::net::SocketAddr,
    cfg: axum_server::tls_rustls::RustlsConfig,
    app: Router,
    stop: S,
    grace: Duration,
) -> Result<()>
where
    S: std::future::Future<Output = ()>,
{
    let handle = axum_server::Handle::new();
    let server = axum_server::bind_rustls(sock, cfg).handle(handle.clone()).serve(app.into_make_service());
    tokio::pin!(server);
    tokio::select! {
        r = &mut server => return r.map_err(|e| anyhow::anyhow!("serve (tls): {e}")),
        _ = stop => {}
    }
    // axum-server stops accepting at once, waits `grace` for open connections, then closes
    // the rest itself; the extra second only backs that up.
    handle.graceful_shutdown(Some(grace));
    drain(server, grace + Duration::from_secs(1)).await
}

/// Set the process umask to 077 and return the previous mask. `dmem serve` creates the tenant
/// databases with their `-wal`/`-shm` files, `iam.db`'s `-wal`/`-shm`, the vector index, the
/// TLS key and anything restored into the data dir; under a common 022 umask most of those
/// were world-readable (the databases themselves are chmod 0600 at open, their siblings and
/// the vector files were not). With 077 everything the server creates is owner-only; only the
/// public TLS certificate is widened again, on purpose (clients copy it).
#[cfg(unix)]
fn restrict_umask() -> u32 {
    // SAFETY: umask(2) only swaps the process file-mode creation mask; it cannot fail.
    unsafe { libc::umask(0o077) as u32 }
}

pub fn run_blocking(addr: &str, tls: TlsOpts, hardening: HardeningOpts) -> Result<()> {
    // First, before any file is created: everything this process writes is owner-only.
    #[cfg(unix)]
    {
        let previous = restrict_umask();
        if previous & 0o077 != 0o077 {
            eprintln!("dmem serve: umask {previous:03o} -> 077 (files the server creates are owner-only)");
        }
    }
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
    let auth = BearerAuth::from_env()?;
    metrics_token_from_env()?;
    // Load the embedding model before serving (and before IAM mints anything), so the FIRST
    // recall does not pay the model load on a request. No-op without the vector feature.
    #[cfg(feature = "zvec")]
    crate::tools::warm_embedder();
    // The model is required (DM_REQUIRE_MODEL): a model that did not load leaves the
    // keyword-equivalent placeholder in its place, and every recall degrades. Opted out
    // (DM_REQUIRE_MODEL=0), the placeholder runs with a warning and /readyz reports it.
    #[cfg(feature = "zvec")]
    let embedder_name = Some(crate::tools::shared_embedder().name());
    #[cfg(not(feature = "zvec"))]
    let embedder_name: Option<&str> = None;
    crate::tools::enforce_model_requirement(embedder_name).map_err(|e| anyhow::anyhow!("refusing to start: {e:#}"))?;
    #[cfg(not(feature = "zvec"))]
    if crate::config::require_vectors() {
        anyhow::bail!("DM_REQUIRE_VECTORS is set, but this dmem build has no vector index (keyword-only); refusing to start");
    }
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
            // DM_ADMIN_TOKEN supplies the admin token, so nothing was minted, printed or
            // written above. A bootstrap token from a start before it was set still works
            // until revoked; point at it without showing it.
            if std::env::var_os("DM_ADMIN_TOKEN").is_some() {
                if let Some(p) = iam.admin_token_path().filter(|p| p.exists()) {
                    eprintln!(
                        "dmem serve: DM_ADMIN_TOKEN is set, but {} holds a bootstrap admin token from an earlier \
                         start; it stays valid until revoked (`dmem admin revoke <that token>`), then delete the file.",
                        p.display()
                    );
                }
            }
            Some(iam)
        }
        Err(e) if hardening.allow_env_only => {
            eprintln!("dmem serve: IAM unavailable ({e:#}); serving with env tokens only (--allow-env-only) - token revocation/suspension and tenant write-scope policies are NOT enforced until IAM is reachable.");
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
    let state = app_state(Arc::new(auth), iam);
    #[cfg(feature = "zvec")]
    {
        eprintln!(
            "dmem serve: embedding at most {} record bod(ies) and {} recall quer(ies) at once \
             (DM_EMBED_CONCURRENCY, DM_EMBED_QUERY_CONCURRENCY)",
            state.embed_gate.saves.total, state.embed_gate.queries.total
        );
        // Not ready until the start-up pass has judged every known tenant.
        state.started.store(false, Ordering::SeqCst);
        let st = state.clone();
        std::thread::spawn(move || background(&st));
    }
    let grace = shutdown_grace();
    let rt = tokio::runtime::Runtime::new()?;
    let served = rt.block_on(async move {
        let app = router_with_state(state.clone());
        // SIGTERM/SIGINT: stop accepting, drain in-flight requests (up to `grace`), then flush
        // and close every tenant (below). Before 0.3.8 a stop was a crash-stop on the TLS path.
        let st = state.clone();
        let stop = async move {
            let name = stop_signal().await;
            st.draining.store(true, Ordering::SeqCst);
            eprintln!(
                "dmem serve: {name} received: accepting no new connections; draining {} in-flight request(s) (up to {}s)",
                st.in_flight.load(Ordering::SeqCst),
                grace.as_secs()
            );
        };
        let sock: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("bad addr {addr}: {e}"))?;

        match tls_config(addr, &tls).await? {
            Some(cfg) => {
                eprintln!("dmem serve: listening on https://{addr}");
                serve_tls(sock, cfg, app, stop, grace).await?;
            }
            None => {
                eprintln!("dmem serve: WARNING serving plain HTTP on http://{addr} (no TLS).");
                eprintln!("           use --tls-cert/--tls-key or --tls-generate for HTTPS.");
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
                serve_plain(listener, app, stop, grace).await?;
            }
        }
        // Drained: flush every open tenant's vector index and close it.
        let st = state.clone();
        let report = tokio::task::spawn_blocking(move || st.close_tenants(Instant::now() + CLOSE_BUDGET))
            .await
            .map_err(|e| anyhow::anyhow!("closing tenants: {e}"))?;
        eprintln!(
            "dmem serve: closed {} tenant(s), vector index flushed for {}",
            report.closed, report.flushed
        );
        for t in &report.opening {
            eprintln!("dmem serve: tenant '{t}' was still opening (the start-up migration resumes next start); not flushed");
        }
        for t in &report.busy {
            eprintln!("dmem serve: tenant '{t}' was still busy after {}s; not flushed", CLOSE_BUDGET.as_secs());
        }
        for (t, e) in &report.failed {
            eprintln!("dmem serve: flushing tenant '{t}' failed ({e})");
        }
        eprintln!("dmem serve: shutdown complete");
        Ok::<(), anyhow::Error>(())
    });
    // A request still running past the grace and the close budget is abandoned here instead of
    // holding the exit open.
    rt.shutdown_timeout(Duration::from_secs(1));
    served
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_signed_sans_cover_bind_host_and_operator_names() {
        use super::self_signed_sans;
        // wildcard bind names nothing: only localhost + the operator's public name
        assert_eq!(self_signed_sans("0.0.0.0:8077", &["dmem.example.net".into()]), vec!["localhost", "dmem.example.net"]);
        // explicit bind host is included; duplicates from --tls-san are dropped
        assert_eq!(self_signed_sans("192.0.2.5:8077", &["192.0.2.5".into(), " ".into()]), vec!["localhost", "192.0.2.5"]);
        assert_eq!(self_signed_sans("localhost:1", &[]), vec!["localhost"]);
    }

    #[test]
    fn loopback_addr_detection() {
        for ok in ["127.0.0.1:8077", "127.9.9.9:80", "localhost:8077", "[::1]:8077"] {
            assert!(is_loopback_addr(ok), "{ok} is loopback");
        }
        for bad in ["0.0.0.0:8077", "198.51.100.64:8077", "203.0.113.1:8077", "memory.example.com:443"] {
            assert!(!is_loopback_addr(bad), "{bad} is not loopback");
        }
    }

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    // These tests mutate process-global env (DM_DATA_DIR, DM_TOKEN_*). Cargo runs tests in a
    // binary multithreaded, so they must serialize on this lock; any future env-reading test
    // in this binary must take it too (through `env_lock`).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// POST a JSON body as `tok` with extra headers; (status, parsed body or Null).
    async fn post_as(
        app: &Router,
        uri: &str,
        tok: &str,
        body: serde_json::Value,
        hdrs: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {tok}"));
        for (k, v) in hdrs {
            b = b.header(*k, *v);
        }
        let resp = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Take `ENV_LOCK`, opted out of the model requirement (`DM_REQUIRE_MODEL=0`), as a
    /// development server would: these tests open tenants on the shared embedder, which is the
    /// placeholder on a host without the model or in a build without one, and none of them is
    /// about the requirement (the process tests are). Set under the lock, in every test, so no
    /// test depends on which ran before it.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("DM_REQUIRE_MODEL", "0");
        g
    }

    /// A fresh isolated data dir + IAM with one tenant: (dir, iam, full-tenant token, adapter token).
    fn scope_fixture(tag: &str) -> (std::path::PathBuf, crate::iam::Iam, String, String) {
        let dir = std::env::temp_dir().join(format!("dm{tag}-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::fs::create_dir_all(&dir).unwrap();
        let iam = crate::iam::Iam::open_at(&dir.join("iam.db")).unwrap();
        let (_t, full) = iam.create_tenant(tag, "", "", None).unwrap();
        let adapter = iam.mint_scoped_token(tag, "bridge", None, &[], None, true).unwrap();
        (dir, iam, full, adapter)
    }

    /// Scopes holding a live row of `uri`, read straight from the tenant db (sorted).
    fn live_scopes_on_disk(dir: &std::path::Path, tenant: &str, uri: &str) -> Vec<String> {
        let c = rusqlite::Connection::open(dir.join("tenants").join(format!("{tenant}.db"))).unwrap();
        let mut st = c.prepare("SELECT scope FROM entries WHERE uri=?1 AND system_to_ms IS NULL ORDER BY scope").unwrap();
        st.query_map([uri], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    // E4 over the wire, on the release embedder when built with it: the daimon-hub integration
    // case `test_room_save_does_not_hide_a_global_record_elsewhere`. A room re-saving a global
    // runbook's kind/namespace/title (same uri, newer row) must not hide the global record from
    // another room - neither in get()-based hydration nor by overwriting its vector. The global
    // body shares the query's words: in hybrid mode cosine is the recall floor and only the
    // body is embedded, so a body unrelated to the query (the hub fixture's bare "operator
    // curated", cosine ~0.53 against the title query) is floored out with no shadow at all.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn room_save_does_not_hide_a_global_record_over_http() {
        let _g = env_lock();
        let (_dir, iam, full, bridge) = scope_fixture("shadowx");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let t = |body: &str| json!({"kind": "runbook", "namespace": "resources/workspace", "title": "zzh shared runbook", "body": body});
        let global_body = "zzh shared runbook: operator curated steps";
        let (st, v) = post_as(&app, "/import", &full, t(global_body), &[]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let g = v["uri"].as_str().unwrap().to_string();
        let room_a = [("x-dm-scopes", "room:abc123"), ("x-dm-write-scope", "room:abc123")];
        let (st, v) = post_as(&app, "/import", &bridge, t("room shadow"), &room_a).await;
        assert_eq!((st, v["uri"].as_str()), (StatusCode::OK, Some(g.as_str())), "same uri: {v}");

        let (st, v) = post_as(&app, "/recall", &bridge, json!({"query": "zzh shared runbook", "limit": 10}), &[("x-dm-scopes", "room:other")]).await;
        assert_eq!(st, StatusCode::OK);
        let hits = v.as_array().cloned().unwrap_or_default();
        assert!(
            hits.iter().any(|h| h["uri"].as_str() == Some(g.as_str()) && h["scope"] == "" && h["body"] == global_body),
            "room b finds the global record: {v}"
        );
        assert!(hits.iter().all(|h| h["scope"] != "room:abc123"), "room a's record never reaches room b: {v}");
        // room a itself still reads its own record under the uri
        let (_, v) = post_as(&app, "/recall", &bridge, json!({"query": "zzh shared runbook room shadow", "limit": 10}), &room_a).await;
        assert!(
            v.as_array().is_some_and(|a| a.iter().any(|h| h["uri"].as_str() == Some(g.as_str()) && h["scope"] == "room:abc123")),
            "{v}"
        );
        std::env::remove_var("DM_DATA_DIR");
    }

    // E1 over the wire: an adapter writing as room a retracts exactly room a's row of a uri
    // room b also holds (the operator-surface case), and gets an error for a uri it holds no
    // row of - never a silent close of another audience's record.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn scoped_forget_over_http_is_scope_exact() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("forgetx");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let note = |body: &str| json!({"kind": "runbook", "namespace": "resources/notes", "title": "zzy shared title", "body": body});
        let room_b = [("x-dm-scopes", "room:b"), ("x-dm-write-scope", "room:b")];
        let room_a = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")];
        let (st, v) = post_as(&app, "/import", &bridge, note("room B notes"), &room_b).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let uri = v["uri"].as_str().unwrap().to_string();
        let (st, v) = post_as(&app, "/import", &bridge, note("room A notes"), &room_a).await;
        assert_eq!((st, v["uri"].as_str()), (StatusCode::OK, Some(uri.as_str())));
        assert_eq!(live_scopes_on_disk(&dir, "forgetx", &uri), vec!["room:a", "room:b"]);

        let (st, v) = post_as(&app, "/forget", &bridge, json!({"uri": uri}), &room_a).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["forgotten"], 1, "room a's row only: {v}");
        assert_eq!(live_scopes_on_disk(&dir, "forgetx", &uri), vec!["room:b"], "room b's record survives");
        // room a has nothing left under this uri: refused
        let (st, _) = post_as(&app, "/forget", &bridge, json!({"uri": uri}), &room_a).await;
        assert_ne!(st, StatusCode::OK);
        assert_eq!(live_scopes_on_disk(&dir, "forgetx", &uri), vec!["room:b"]);
        // invalidate follows the same rule
        let cut = crate::entry::now_ms() + 60_000;
        let (st, _) = post_as(&app, "/invalidate", &bridge, json!({"uri": uri, "valid_to": cut}), &room_a).await;
        assert_ne!(st, StatusCode::OK, "room a cannot cut room b's segment");
        // the full-tenant token is unchanged: it closes whatever is live
        let (st, v) = post_as(&app, "/forget", &full, json!({"uri": uri}), &[]).await;
        assert_eq!((st, v["forgotten"].as_u64()), (StatusCode::OK, Some(1)));
        assert!(live_scopes_on_disk(&dir, "forgetx", &uri).is_empty());
        std::env::remove_var("DM_DATA_DIR");
    }

    // E5 over the wire: an adapter writing as room a links its own record to a global one, but
    // cannot wire two global records together, nor create a free-text rel; it can remove a
    // legacy free-text edge. Refusals answer 400 with the rule that refused. The full-tenant
    // token is unchanged.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn edge_ownership_over_http() {
        let _g = env_lock();
        let (_dir, iam, full, bridge) = scope_fixture("edgex");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let rec = |title: &str| json!({"kind": "runbook", "namespace": "resources/workspace", "title": title, "body": format!("{title} body")});
        let room_a = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")];
        let uri_of = |v: &serde_json::Value| v["uri"].as_str().unwrap().to_string();
        let (_, v) = post_as(&app, "/import", &full, rec("global one"), &[]).await;
        let g1 = uri_of(&v);
        let (_, v) = post_as(&app, "/import", &full, rec("global two"), &[]).await;
        let g2 = uri_of(&v);
        let (st, v) = post_as(&app, "/import", &bridge, rec("room a own"), &room_a).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let own = uri_of(&v);

        let (st, v) = post_as(&app, "/link", &bridge, json!({"from": own, "to": g1, "rel": "about"}), &room_a).await;
        assert_eq!((st, &v["linked"]), (StatusCode::OK, &json!(1)), "own -> global is allowed: {v}");
        let (st, v) = post_as(&app, "/link", &bridge, json!({"from": g1, "to": g2, "rel": "about"}), &room_a).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().is_some_and(|m| m.contains("only this identity's write scope holds")), "{v}");
        let (st, v) = post_as(&app, "/link", &bridge, json!({"from": own, "to": g2, "rel": "Relates To"}), &room_a).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().is_some_and(|m| m.starts_with("invalid rel")), "{v}");

        // the full-tenant token keeps any label and any endpoints
        let (st, _) = post_as(&app, "/link", &full, json!({"from": g1, "to": g2, "rel": "about"}), &[]).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = post_as(&app, "/link", &full, json!({"from": own, "to": g2, "rel": "legacy free text"}), &[]).await;
        assert_eq!(st, StatusCode::OK);
        // room a may not cut the operator's global edge, but may remove the free-text edge on
        // its own record
        let (st, _) = post_as(&app, "/unlink", &bridge, json!({"from": g1, "to": g2, "rel": "about"}), &room_a).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = post_as(&app, "/unlink", &bridge, json!({"from": own, "to": g2, "rel": "legacy free text"}), &room_a).await;
        assert_eq!((st, &v["unlinked"]), (StatusCode::OK, &json!(1)), "{v}");
        let (_, v) = post_as(&app, "/edges", &full, json!({"uri": g1}), &[]).await;
        assert_eq!(v.as_array().map(|a| a.len()), Some(2), "own->g1 and g1->g2 remain: {v}");
        std::env::remove_var("DM_DATA_DIR");
    }

    fn id(scope_read: Option<Vec<String>>, scope_write: Option<&str>, adapter: bool) -> crate::iam::Identity {
        crate::iam::Identity {
            tenant: Some("t".into()),
            is_admin: false,
            agent: None,
            scope_read,
            scope_write: scope_write.map(str::to_string),
            adapter,
        }
    }

    fn hdrs(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn scope_binding_applies_the_explicit_write_scope_policy_to_unbound_tokens_only() {
        let b = |w: Option<&str>, r: Option<Vec<&str>>, missing: bool| ScopeBinding {
            write: w.map(str::to_string),
            read: r.map(|v| v.into_iter().map(str::to_string).collect()),
            write_refusal: missing.then(|| crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED.to_string()),
        };
        let full = id(None, None, false);
        // no policy, no header: global writer, full tenant - the pre-scope server
        assert_eq!(scope_binding(&full, &hdrs(&[]), false), b(Some(""), None, false));
        // policy, no header: writes refused with the policy reason, reads unchanged
        assert_eq!(scope_binding(&full, &hdrs(&[]), true), b(None, None, true));
        // the header names the scope, with or without the policy; X-DM-Scopes stays ignored
        for policy in [false, true] {
            assert_eq!(scope_binding(&full, &hdrs(&[("x-dm-write-scope", "")]), policy), b(Some(""), None, false));
            let h = hdrs(&[("x-dm-write-scope", " room:x "), ("x-dm-scopes", "user:bob")]);
            assert_eq!(scope_binding(&full, &h, policy), b(Some("room:x"), None, false));
        }
        // scope-bound tokens are untouched by the policy (they never write global by default)
        let bridge = id(None, None, true);
        assert_eq!(scope_binding(&bridge, &hdrs(&[]), true), b(None, Some(vec![]), false), "adapter without header: read-only, old reason");
        let h = hdrs(&[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")]);
        assert_eq!(scope_binding(&bridge, &h, true), b(Some("room:a"), Some(vec!["room:a"]), false));
        let alice = id(Some(vec!["user:alice".into()]), Some("user:alice"), false);
        let h = hdrs(&[("x-dm-write-scope", "")]);
        assert_eq!(scope_binding(&alice, &h, true), b(Some("user:alice"), Some(vec!["user:alice"]), false), "static grants: header ignored");
    }

    /// A header value the server cannot read as text (bytes >= 0x80 pass HeaderValue but not
    /// to_str()).
    fn raw_hdr(name: &'static str, bytes: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, axum::http::HeaderValue::from_bytes(bytes).unwrap());
        h
    }

    #[test]
    fn an_unusable_write_scope_header_never_reads_as_absent() {
        // Before: to_str() failed on `room:caf\xc3\xa9`, the header counted as absent, and a
        // full-tenant token on a non-policy tenant wrote GLOBAL (a policy tenant said "requires
        // an explicit write scope"). Now: Invalid, and the binding refuses writes.
        let bad = [
            raw_hdr("x-dm-write-scope", "room:caf\u{e9}".as_bytes()),
            hdrs(&[("x-dm-write-scope", "room:a,room:b")]),
            {
                let mut h = hdrs(&[("x-dm-write-scope", "room:a")]);
                h.append("x-dm-write-scope", "room:b".parse().unwrap());
                h
            },
        ];
        for h in &bad {
            assert_eq!(write_scope_header(h), WriteScopeHeader::Invalid, "{h:?}");
            for policy in [false, true] {
                let got = scope_binding(&id(None, None, false), h, policy);
                assert_eq!((got.write, got.read), (None, None), "{h:?}: no write scope, full-tenant reads");
                assert_eq!(got.write_refusal.as_deref(), Some(INVALID_WRITE_SCOPE));
            }
            let got = scope_binding(&id(None, None, true), h, false);
            assert_eq!((got.write, got.write_refusal.as_deref()), (None, Some(INVALID_WRITE_SCOPE)), "adapters too");
        }
        assert_eq!(write_scope_header(&hdrs(&[])), WriteScopeHeader::Absent);
        assert_eq!(write_scope_header(&hdrs(&[("x-dm-write-scope", " room:x ")])), WriteScopeHeader::Named("room:x".into()));
        assert_eq!(write_scope_header(&hdrs(&[("x-dm-write-scope", "")])), WriteScopeHeader::Named(String::new()));
    }

    #[test]
    fn a_capped_adapter_writes_only_inside_its_cap() {
        let r = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let capped = id(Some(vec!["room:a".into(), "room:b".into()]), None, true);
        // inside the cap: stamped, and the read set stays within the cap
        let h = hdrs(&[("x-dm-scopes", "room:a,user:bob"), ("x-dm-write-scope", "room:a")]);
        let got = scope_binding(&capped, &h, false);
        assert_eq!((got.write.as_deref(), got.read, got.write_refusal), (Some("room:a"), r(&["room:a"]), None));
        // outside the cap: read-only with the reason, and the named scope is NOT readable
        // (before: write user:bob and read [room:a, user:bob] - any scope, just by naming it)
        let h = hdrs(&[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "user:bob")]);
        let got = scope_binding(&capped, &h, false);
        assert_eq!((got.write, got.read), (None, r(&["room:a"])));
        assert_eq!(got.write_refusal.as_deref(), Some("write scope 'user:bob' is outside this adapter token's scope cap"));
        // global promotion is an uncapped adapter's job
        let h = hdrs(&[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")]);
        let got = scope_binding(&capped, &h, false);
        assert_eq!((got.write, got.read), (None, r(&["room:a"])));
        assert!(got.write_refusal.is_some_and(|m| m.contains("does not cover global writes")));
        // no header: read-only with the plain reason, as before
        let got = scope_binding(&capped, &hdrs(&[("x-dm-scopes", "room:b")]), false);
        assert_eq!((got.write, got.read, got.write_refusal), (None, r(&["room:b"]), None));
        // an uncapped adapter is unchanged: any scope, '' promotes (a read-restricted global
        // writer, which the engine bounds to the global rows)
        let open = id(None, None, true);
        let got = scope_binding(&open, &hdrs(&[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "user:bob")]), false);
        assert_eq!((got.write.as_deref(), got.read), (Some("user:bob"), r(&["room:a", "user:bob"])));
        let got = scope_binding(&open, &hdrs(&[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")]), false);
        assert_eq!((got.write.as_deref(), got.read), (Some(""), r(&["room:a"])));
    }

    /// Scope and body of every live row whose body is `body`, straight from the tenant db.
    fn live_rows_with_body(dir: &std::path::Path, tenant: &str, body: &str) -> Vec<String> {
        let c = rusqlite::Connection::open(dir.join("tenants").join(format!("{tenant}.db"))).unwrap();
        let mut st = c.prepare("SELECT scope FROM entries WHERE body=?1 AND system_to_ms IS NULL ORDER BY scope").unwrap();
        st.query_map([body], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    // E6 over the wire: a tenant with `require_explicit_write_scope` refuses full-token writes
    // that name no scope (400 with the policy message, nothing saved), honours '' (global) and
    // a named scope, and leaves reads, scoped tokens and other tenants alone. The policy is an
    // admin setting that applies from the next request.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn explicit_write_scope_policy_over_http() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("policyx");
        let admin = iam.ensure_admin_from(None).unwrap().expect("a fresh data dir mints a bootstrap admin token");
        let (_t, other_full) = iam.create_tenant("policyy", "", "", None).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let remember = |text: &str| json!({"text": text, "namespace": "resources/notes"});
        const POLICY_MSG: &str = crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED;

        // policy off (the default): unchanged
        let (st, _) = post_as(&app, "/remember", &full, remember("zq before the policy"), &[]).await;
        assert_eq!(st, StatusCode::OK);
        // only the admin sets the policy
        let set = |on: bool| json!({"tenant": "PolicyX", "require_explicit_write_scope": on});
        let (st, _) = post_as(&app, "/admin/tenant_policy", &full, set(true), &[]).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, v) = post_as(&app, "/admin/tenant_policy", &admin, set(true), &[]).await;
        assert_eq!((st, &v["tenant"], &v["require_explicit_write_scope"]), (StatusCode::OK, &json!("policyx"), &json!(true)), "{v}");
        let (st, v) = post_as(&app, "/admin/tenant_policy", &admin, json!({"tenant": "nosuch", "require_explicit_write_scope": true}), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

        // no header: 400 with the policy message, and nothing saved
        let (st, v) = post_as(&app, "/remember", &full, remember("zq no header"), &[]).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(POLICY_MSG)), "{v}");
        assert!(live_rows_with_body(&dir, "policyx", "zq no header").is_empty());
        // '' = a deliberate global write; a named scope is stamped
        let (st, v) = post_as(&app, "/remember", &full, remember("zq explicit global"), &[("x-dm-write-scope", "")]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq explicit global"), vec![""]);
        let (st, v) = post_as(&app, "/remember", &full, remember("zq room x note"), &[("x-dm-write-scope", "room:x")]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq room x note"), vec!["room:x"]);
        let room_uri = v["uri"].as_str().unwrap().to_string();

        // every write route refuses the same way; reads are unchanged (full tenant, no header)
        let typed = json!({"kind": "runbook", "namespace": "resources/notes", "title": "zq typed", "body": "b"});
        for (path, body) in [
            ("/import", typed),
            ("/log_decision", json!({"title": "zq d", "decision": "x"})),
            ("/forget", json!({"uri": room_uri})),
            ("/invalidate", json!({"uri": room_uri, "valid_to": crate::entry::now_ms() + 60_000})),
            ("/link", json!({"from": room_uri, "to": room_uri, "rel": "about"})),
        ] {
            let (st, v) = post_as(&app, path, &full, body, &[]).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(POLICY_MSG)), "{path}: {v}");
        }
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq room x note"), vec!["room:x"], "nothing retracted");
        let (st, v) = post_as(&app, "/history", &full, json!({"uri": room_uri}), &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v.as_array().is_some_and(|a| a.len() == 1 && a[0]["scope"] == "room:x"), "full-tenant read of a room row: {v}");

        // MCP over HTTP: the write tool reports the refusal in-band, and works with the header
        let mcp = |text: &str| json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "remember", "arguments": {"text": text}}});
        let (st, v) = post_as(&app, "/mcp", &full, mcp("zq via mcp"), &[]).await;
        assert_eq!((st, &v["result"]["isError"]), (StatusCode::OK, &json!(true)), "{v}");
        assert!(v["result"]["content"][0]["text"].as_str().is_some_and(|t| t.contains(POLICY_MSG)), "{v}");
        let (_, v) = post_as(&app, "/mcp", &full, mcp("zq via mcp"), &[("x-dm-write-scope", "room:x")]).await;
        assert_eq!(v["result"]["isError"], json!(false), "{v}");
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq via mcp"), vec!["room:x"]);

        // the adapter keeps its own rules: read-only without a header (old reason), stamped with one
        let (st, v) = post_as(&app, "/remember", &bridge, remember("zq bridge none"), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().is_some_and(|m| m.starts_with("this identity has no write scope")), "{v}");
        let (st, _) = post_as(&app, "/remember", &bridge, remember("zq bridge room"), &[("x-dm-write-scope", "room:b")]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq bridge room"), vec!["room:b"]);

        // a non-policy tenant is unchanged: no header lands global; a named scope is honoured
        let (st, _) = post_as(&app, "/remember", &other_full, remember("zq other tenant"), &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(live_rows_with_body(&dir, "policyy", "zq other tenant"), vec![""]);
        let (st, _) = post_as(&app, "/remember", &other_full, remember("zq other scoped"), &[("x-dm-write-scope", "room:y")]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(live_rows_with_body(&dir, "policyy", "zq other scoped"), vec!["room:y"]);

        // the admin listing shows the policy; turning it off applies to the next request
        let list = Request::builder().method("GET").uri("/admin/tenants").header("authorization", format!("Bearer {admin}")).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(list).await.unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        let flag = |t: &str| rows.as_array().unwrap().iter().find(|r| r["tenant"] == t).unwrap()["require_explicit_write_scope"].clone();
        assert_eq!((flag("policyx"), flag("policyy")), (json!(true), json!(false)), "{rows}");
        let (st, v) = post_as(&app, "/admin/tenant_policy", &admin, json!({"tenant": "policyx"}), &[]).await;
        assert_eq!((st, &v["require_explicit_write_scope"]), (StatusCode::OK, &json!(true)), "read without a change: {v}");
        let (st, _) = post_as(&app, "/admin/tenant_policy", &admin, set(false), &[]).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = post_as(&app, "/remember", &full, remember("zq after the policy"), &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(live_rows_with_body(&dir, "policyx", "zq after the policy"), vec![""]);
        std::env::remove_var("DM_DATA_DIR");
    }

    /// Every row (live or closed) stored under `uri`, as (scope, importance, created_ms).
    fn rows_on_disk(dir: &std::path::Path, tenant: &str, uri: &str) -> Vec<(String, i64, i64)> {
        let c = rusqlite::Connection::open(dir.join("tenants").join(format!("{tenant}.db"))).unwrap();
        let mut st = c.prepare("SELECT scope, importance, created_ms FROM entries WHERE uri=?1 ORDER BY id").unwrap();
        st.query_map([uri], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    // E7 over the wire: the /import migration form (created_ms / importance) goes through the
    // scope and namespace guards. Before, it ignored the write scope and landed GLOBAL, which
    // let an adapter plant a high-importance rule for every room.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn import_migration_form_goes_through_the_guards_over_http() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("importx");
        let alice = iam
            .mint_scoped_token("importx", "alice", None, &["user:alice".to_string()], Some("user:alice"), false)
            .unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let room_a = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")];
        let imp = |kind: &str, ns: &str, title: &str, extra: serde_json::Value| {
            let mut b = json!({"kind": kind, "namespace": ns, "title": title, "body": "b"});
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            b
        };
        let uri = |ns: &str, kind: Kind, title: &str| crate::entry::make_uri(ns, kind, title);
        use crate::entry::Kind;
        let t0: i64 = 1_600_000_000_000;

        // adapter as room a, importance above the kind default: 400, and never global
        let (st, v) = post_as(&app, "/import", &bridge, imp("chunk", "resources/docs", "zi raised", json!({"importance": 50})), &room_a).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("importance is set by the operator")), "{v}");
        let (st, _) = post_as(&app, "/import", &bridge, imp("protocol", "agent/protocol", "zi rule", json!({"importance": 99})), &room_a).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(rows_on_disk(&dir, "importx", &uri("resources/docs", Kind::Chunk, "zi raised")).is_empty());
        assert!(rows_on_disk(&dir, "importx", &uri("agent/protocol", Kind::Protocol, "zi rule")).is_empty());
        // at or below the default: lands in room a (the ignored-scope bug put it in global)
        let (st, v) = post_as(&app, "/import", &bridge, imp("memory", "resources/notes", "zi low", json!({"importance": 50})), &room_a).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(rows_on_disk(&dir, "importx", &uri("resources/notes", Kind::Memory, "zi low")).iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), vec![("room:a", 50)]);

        // adapter with no write scope plus created_ms: 400, nothing saved
        let (st, v) = post_as(&app, "/import", &bridge, imp("memory", "resources/notes", "zi old", json!({"created_ms": t0})), &[("x-dm-scopes", "room:a")]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(rows_on_disk(&dir, "importx", &uri("resources/notes", Kind::Memory, "zi old")).is_empty());
        // with a write scope, backdating is still the operator's
        let (st, v) = post_as(&app, "/import", &bridge, imp("memory", "resources/notes", "zi old", json!({"created_ms": t0})), &room_a).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("created_ms is set by the operator")), "{v}");
        let (st, _) = post_as(&app, "/import", &alice, imp("memory", "resources/notes", "zi old", json!({"created_ms": t0})), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a plain scoped token too");
        assert!(rows_on_disk(&dir, "importx", &uri("resources/notes", Kind::Memory, "zi old")).is_empty());
        // a scoped identity importing into another agent's tree via created_ms: 400
        let (st, _) = post_as(&app, "/import", &bridge, imp("persona", "agents/analyst", "zi planted", json!({"created_ms": t0})), &room_a).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = post_as(&app, "/import", &alice, imp("persona", "agents/analyst", "zi planted", json!({"created_ms": t0, "importance": 95})), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(rows_on_disk(&dir, "importx", &uri("agents/analyst", Kind::Persona, "zi planted")).is_empty());

        // the full-tenant token is unchanged: the migration keeps created_ms and importance
        let (st, v) = post_as(&app, "/import", &full, imp("protocol", "agent/protocol", "zi migrated", json!({"created_ms": t0, "importance": 99})), &[]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(rows_on_disk(&dir, "importx", &uri("agent/protocol", Kind::Protocol, "zi migrated")), vec![(String::new(), 99, t0)]);
        std::env::remove_var("DM_DATA_DIR");
    }

    // E7 with E6 on: the full-tenant migration import needs the explicit header too.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn import_migration_form_needs_the_explicit_write_scope_on_a_policy_tenant() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("importp");
        iam.set_requires_explicit_write_scope("importp", true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let t0: i64 = 1_600_000_000_000;
        let body = json!({"kind": "memory", "namespace": "resources/notes", "title": "zp migrated", "body": "b", "created_ms": t0, "importance": 80});
        let u = crate::entry::make_uri("resources/notes", crate::entry::Kind::Memory, "zp migrated");
        let (st, v) = post_as(&app, "/import", &full, body.clone(), &[]).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED)), "{v}");
        assert!(rows_on_disk(&dir, "importp", &u).is_empty());
        let (st, v) = post_as(&app, "/import", &full, body.clone(), &[("x-dm-write-scope", "")]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(rows_on_disk(&dir, "importp", &u), vec![(String::new(), 80, t0)], "'' named: the migration as before");
        // a full-tenant token that names a room writes AS that room, and keeps the operator
        // levers there (0.3.8; 0.3.7 refused them)
        let (st, v) = post_as(&app, "/import", &full, body, &[("x-dm-write-scope", "room:x")]).await;
        assert_eq!((st, v["scope"].as_str()), (StatusCode::OK, Some("room:x")), "{v}");
        assert_eq!(rows_on_disk(&dir, "importp", &u), vec![(String::new(), 80, t0), ("room:x".to_string(), 80, t0)]);
        let backdated = json!({"kind": "memory", "namespace": "resources/notes", "title": "zp room", "body": "b", "created_ms": t0});
        let room = crate::entry::make_uri("resources/notes", crate::entry::Kind::Memory, "zp room");
        // an adapter writing as the room still may not backdate
        let (st, v) = post_as(&app, "/import", &bridge, backdated.clone(), &[("x-dm-scopes", "room:x"), ("x-dm-write-scope", "room:x")]).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("created_ms is set by the operator")), "{v}");
        assert!(rows_on_disk(&dir, "importp", &room).is_empty());
        let (st, v) = post_as(&app, "/import", &full, backdated, &[("x-dm-write-scope", "room:x")]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(rows_on_disk(&dir, "importp", &room), vec![("room:x".to_string(), 60, t0)]);
        std::env::remove_var("DM_DATA_DIR");
    }

    // The operator's import into a named scope: a scope-unbound full-tenant token that names a
    // room in `X-DM-Write-Scope` (`dmem --scope room:a migrate`) keeps both migration levers,
    // and the row is stamped with exactly that room. The same token could already write the
    // record global, which reaches further. Scope-bound tokens and adapters writing as the
    // room are refused as before, and nothing else widens: the `agents/` tree stays closed to
    // a room writer.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn operator_import_keeps_the_migration_levers_in_a_named_scope_over_http() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("importo");
        let alice = iam.mint_scoped_token("importo", "alice", None, &["room:a".to_string()], Some("room:a"), false).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        use crate::entry::Kind;
        let imp = |kind: &str, ns: &str, title: &str, extra: serde_json::Value| {
            let mut b = json!({"kind": kind, "namespace": ns, "title": title, "body": "b"});
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            b
        };
        let uri = |kind: Kind, title: &str| crate::entry::make_uri("resources/notes", kind, title);
        let valid_from = |u: &str| -> i64 {
            let c = rusqlite::Connection::open(dir.join("tenants").join("importo.db")).unwrap();
            c.query_row("SELECT valid_from_ms FROM entries WHERE uri=?1", [u], |r| r.get(0)).unwrap()
        };
        let t0: i64 = 1_600_000_000_000;
        let room_a = [("x-dm-write-scope", "room:a")];

        // backdated into room a: the row is room a's, with the original creation and valid time
        let (st, v) = post_as(&app, "/import", &full, imp("memory", "resources/notes", "zo dated", json!({"created_ms": t0})), &room_a).await;
        assert_eq!((st, v["scope"].as_str()), (StatusCode::OK, Some("room:a")), "{v}");
        let dated = uri(Kind::Memory, "zo dated");
        assert_eq!(rows_on_disk(&dir, "importo", &dated), vec![("room:a".to_string(), 60, t0)]);
        assert_eq!(valid_from(&dated), t0);
        // importance above the kind default: kept
        let (st, v) = post_as(&app, "/import", &full, imp("memory", "resources/notes", "zo raised", json!({"importance": 95})), &room_a).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let raised = rows_on_disk(&dir, "importo", &uri(Kind::Memory, "zo raised"));
        assert_eq!(raised.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), vec![("room:a", 95)]);
        // both levers at once
        let (st, v) = post_as(&app, "/import", &full, imp("memory", "resources/notes", "zo both", json!({"created_ms": t0, "importance": 90})), &room_a).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(rows_on_disk(&dir, "importo", &uri(Kind::Memory, "zo both")), vec![("room:a".to_string(), 90, t0)]);

        // a scope-bound token writing room a, and an adapter asserting room a: still refused
        let adapter = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")];
        for (who, tok, hdrs) in [("scoped token", &alice, &[][..]), ("adapter", &bridge, &adapter[..])] {
            let raised = format!("zo {who} raised");
            let (st, v) = post_as(&app, "/import", tok, imp("memory", "resources/notes", &raised, json!({"importance": 95})), hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("importance is set by the operator")), "{who}: {v}");
            let dated = format!("zo {who} dated");
            let (st, v) = post_as(&app, "/import", tok, imp("memory", "resources/notes", &dated, json!({"created_ms": t0})), hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("created_ms is set by the operator")), "{who}: {v}");
            assert!(rows_on_disk(&dir, "importo", &uri(Kind::Memory, &raised)).is_empty(), "{who}");
            assert!(rows_on_disk(&dir, "importo", &uri(Kind::Memory, &dated)).is_empty(), "{who}");
        }

        // only the E7 levers widen: the operator writing as a room stays out of the agents/ tree
        let (st, v) = post_as(&app, "/import", &full, imp("persona", "agents/analyst", "zo planted", json!({"created_ms": t0})), &room_a).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("scoped identities cannot write into the agents/ tree")), "{v}");
        assert!(rows_on_disk(&dir, "importo", &crate::entry::make_uri("agents/analyst", Kind::Persona, "zo planted")).is_empty());

        // the full-tenant token writing global ('' named, or no header): unchanged
        for (title, hdrs) in [("zo global named", &[("x-dm-write-scope", "")][..]), ("zo global default", &[][..])] {
            let (st, v) = post_as(&app, "/import", &full, imp("protocol", "agent/protocol", title, json!({"created_ms": t0, "importance": 99})), hdrs).await;
            assert_eq!((st, v["scope"].as_str()), (StatusCode::OK, Some("")), "{title}: {v}");
            let u = crate::entry::make_uri("agent/protocol", Kind::Protocol, title);
            assert_eq!(rows_on_disk(&dir, "importo", &u), vec![(String::new(), 99, t0)], "{title}");
        }
        std::env::remove_var("DM_DATA_DIR");
    }

    // E6 end to end over TCP: the remote client's write scope (`--scope` / DM_WRITE_SCOPE)
    // travels as X-DM-Write-Scope and satisfies the tenant policy; without it the client
    // surfaces the server's 400 and its message.
    #[cfg(feature = "client")]
    #[test]
    fn remote_client_write_scope_satisfies_the_policy_end_to_end() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("policyc");
        iam.set_requires_explicit_write_scope("policyc", true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = std::thread::spawn(move || {
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app)
                    .with_graceful_shutdown(async move {
                        let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
                    })
                    .await
                    .unwrap();
            })
        });
        let link = crate::config::ServerLink { url: format!("http://{addr}"), token: full, insecure: false, ca_cert: None };
        let client = |ws: Option<&str>| {
            crate::client::RemoteClient::new(&link).unwrap().with_write_scope(ws.map(str::to_string)).unwrap()
        };

        let e = client(None).remember("zq client unscoped", "resources/notes", None, None).expect_err("policy refuses");
        let msg = format!("{e:#}");
        assert!(msg.contains("400") && msg.contains(crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED), "{msg}");
        client(Some("room:x")).remember("zq client room", "resources/notes", None, None).unwrap();
        client(Some("")).remember("zq client global", "resources/notes", None, None).unwrap();
        // reads carry the header too and stay full-tenant
        let rows = client(Some("room:x")).recent(50).unwrap();
        assert!(rows.iter().any(|h| h.scope.is_empty()) && rows.iter().any(|h| h.scope == "room:x"), "{rows:?}");
        let _ = stop_tx.send(());
        server.join().unwrap();
        assert!(live_rows_with_body(&dir, "policyc", "zq client unscoped").is_empty());
        assert_eq!(live_rows_with_body(&dir, "policyc", "zq client room"), vec!["room:x"]);
        assert_eq!(live_rows_with_body(&dir, "policyc", "zq client global"), vec![""]);
        std::env::remove_var("DM_DATA_DIR");
    }

    // A read-restricted GLOBAL writer over the wire, in both forms: an adapter promoting with
    // `X-DM-Write-Scope: ''` while asserting a restricted `X-DM-Scopes`, and a plain token
    // minted with a '' write scope. Before, the engine took either for a full-tenant writer:
    // it retracted and cut records it cannot read, planted operator-importance and backdated
    // records, and wrote into an agent's persona tree.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn read_restricted_global_writers_are_bounded_over_http() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("rgwx");
        let alice_g = iam.mint_scoped_token("rgwx", "alice-global", None, &["user:alice".to_string()], Some(""), false).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let rec = |title: &str, body: &str| json!({"kind": "runbook", "namespace": "resources/workspace", "title": title, "body": body});
        let room_b = [("x-dm-scopes", "room:b"), ("x-dm-write-scope", "room:b")];
        let bob = [("x-dm-scopes", "user:bob"), ("x-dm-write-scope", "user:bob")];
        let (_, v) = post_as(&app, "/import", &bridge, rec("zr hidden", "room b salary review"), &room_b).await;
        let hidden = v["uri"].as_str().unwrap().to_string();
        post_as(&app, "/import", &bridge, rec("zr hidden", "bob salary review"), &bob).await;
        let (_, v) = post_as(&app, "/import", &bridge, rec("zr b one", "b1"), &room_b).await;
        let b1 = v["uri"].as_str().unwrap().to_string();
        let (_, v) = post_as(&app, "/import", &bridge, rec("zr b two", "b2"), &room_b).await;
        let b2 = v["uri"].as_str().unwrap().to_string();
        let (st, _) = post_as(&app, "/link", &full, json!({"from": b1, "to": b2, "rel": "about"}), &[]).await;
        assert_eq!(st, StatusCode::OK);
        let t0: i64 = 1_600_000_000_000;

        let promote = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")];
        for (who, tok, hdrs) in [("adapter", &bridge, &promote[..]), ("token", &alice_g, &[][..])] {
            let (st, v) = post_as(&app, "/forget", tok, json!({"uri": hidden}), hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("record not found in this identity's write scope")), "{who}: {v}");
            let (st, _) = post_as(&app, "/invalidate", tok, json!({"uri": hidden, "valid_to": crate::entry::now_ms() + 60_000}), hdrs).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{who}");
            for path in ["/unlink", "/link"] {
                let (st, v) = post_as(&app, path, tok, json!({"from": b1, "to": b2, "rel": "about"}), hdrs).await;
                assert_eq!(st, StatusCode::BAD_REQUEST, "{who} {path}: {v}");
            }
            let planted = json!({"kind": "protocol", "namespace": "agent/protocol", "title": "zr planted", "body": "b", "importance": 99});
            let (st, v) = post_as(&app, "/import", tok, planted, hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("importance is set by the operator")), "{who}");
            let old = json!({"kind": "memory", "namespace": "resources/notes", "title": "zr old", "body": "b", "created_ms": t0});
            let (st, v) = post_as(&app, "/import", tok, old, hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("created_ms is set by the operator")), "{who}");
            let persona = json!({"kind": "persona", "namespace": "agents/analyst", "title": "zr analyst persona", "body": "I am the analyst"});
            let (st, v) = post_as(&app, "/import", tok, persona, hdrs).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("scoped identities cannot write into the agents/ tree")), "{who}");
            // what it may do: save global (the promotion path), and the reply says so
            let (st, v) = post_as(&app, "/remember", tok, json!({"text": format!("zr promoted by {who}")}), hdrs).await;
            assert_eq!((st, v["scope"].as_str()), (StatusCode::OK, Some("")), "{who}: {v}");
            assert_eq!(live_rows_with_body(&dir, "rgwx", &format!("zr promoted by {who}")), vec![""]);
        }
        assert_eq!(live_scopes_on_disk(&dir, "rgwx", &hidden), vec!["room:b", "user:bob"], "nothing retracted");
        let (_, v) = post_as(&app, "/edges", &full, json!({"uri": b1}), &[]).await;
        assert_eq!(v.as_array().map(|a| a.len()), Some(1), "the edge between room b's records stays: {v}");
        for uri in [
            crate::entry::make_uri("agent/protocol", crate::entry::Kind::Protocol, "zr planted"),
            crate::entry::make_uri("resources/notes", crate::entry::Kind::Memory, "zr old"),
            crate::entry::make_uri("agents/analyst", crate::entry::Kind::Persona, "zr analyst persona"),
        ] {
            assert!(rows_on_disk(&dir, "rgwx", &uri).is_empty(), "{uri}");
        }
        std::env::remove_var("DM_DATA_DIR");
    }

    // A capped adapter over the wire: its token cap bounds its writes as well as its reads.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_capped_adapter_stays_inside_its_cap_over_http() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("capx");
        let capped = iam.mint_scoped_token("capx", "capped", None, &["room:a".to_string()], None, true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let (st, _) = post_as(&app, "/remember", &full, json!({"text": "zc bob private salary"}), &[("x-dm-write-scope", "user:bob")]).await;
        assert_eq!(st, StatusCode::OK);
        let as_bob = [("x-dm-scopes", "room:a"), ("x-dm-write-scope", "user:bob")];
        let (st, v) = post_as(&app, "/remember", &capped, json!({"text": "zc planted in bob"}), &as_bob).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("write scope 'user:bob' is outside this adapter token's scope cap")), "{v}");
        assert!(live_rows_with_body(&dir, "capx", "zc planted in bob").is_empty());
        let (st, v) = post_as(&app, "/recall", &capped, json!({"query": "zc bob private salary", "limit": 10}), &as_bob).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v.as_array().is_some_and(|a| a.iter().all(|h| h["scope"] != "user:bob")), "naming user:bob does not make it readable: {v}");
        let (st, v) = post_as(&app, "/remember", &capped, json!({"text": "zc promoted"}), &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().is_some_and(|m| m.contains("global writes")), "{v}");
        let (st, v) = post_as(&app, "/remember", &capped, json!({"text": "zc room a note"}), &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")]).await;
        assert_eq!((st, v["scope"].as_str()), (StatusCode::OK, Some("room:a")), "{v}");
        std::env::remove_var("DM_DATA_DIR");
    }

    // An X-DM-Write-Scope the server cannot use answers 400 on every route and token type, and
    // never falls back to the token's default scope (global for a full-tenant token).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_unusable_write_scope_header_is_refused_over_http() {
        let _g = env_lock();
        let (dir, iam, full, bridge) = scope_fixture("badhdr");
        let (_t, policy_full) = iam.create_tenant("badhdp", "", "", None).unwrap();
        iam.set_requires_explicit_write_scope("badhdp", true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        // a valid header opens both tenants, so the refusals below are checked on disk
        for tok in [&full, &policy_full] {
            let (st, _) = post_as(&app, "/remember", tok, json!({"text": "zb valid note"}), &[("x-dm-write-scope", "room:ok")]).await;
            assert_eq!(st, StatusCode::OK);
        }
        for value in ["room:caf\u{e9}", "room:a,room:b"] {
            for (path, body) in [("/remember", json!({"text": "zb cafe note"})), ("/recall", json!({"query": "zb cafe"}))] {
                let (st, v) = post_as(&app, path, &full, body, &[("x-dm-write-scope", value)]).await;
                assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(INVALID_WRITE_SCOPE)), "{value:?} {path}: {v}");
            }
            let (st, v) = post_as(&app, "/remember", &policy_full, json!({"text": "zb cafe note"}), &[("x-dm-write-scope", value)]).await;
            assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(INVALID_WRITE_SCOPE)), "not the policy message: {v}");
            let (st, _) = post_as(&app, "/remember", &bridge, json!({"text": "zb cafe note"}), &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", value)]).await;
            assert_eq!(st, StatusCode::BAD_REQUEST);
        }
        assert!(live_rows_with_body(&dir, "badhdr", "zb cafe note").is_empty(), "never stored global");
        assert!(live_rows_with_body(&dir, "badhdp", "zb cafe note").is_empty());
        std::env::remove_var("DM_DATA_DIR");
    }

    // Batch graph maintenance is full-tenant only on every surface: MCP's reindex_links used to
    // run for any token (an adapter, a read-only one, a full token on a policy tenant that
    // named no scope) and wire records of other audiences together.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn reindex_is_full_tenant_only_over_mcp_and_rest() {
        let _g = env_lock();
        let (_dir, iam, full, bridge) = scope_fixture("reidx");
        iam.set_requires_explicit_write_scope("reidx", true).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let global = [("x-dm-write-scope", "")];
        let (_, v) = post_as(&app, "/remember", &full, json!({"text": "zk target note"}), &global).await;
        let target = v["uri"].as_str().unwrap().to_string();
        post_as(&app, "/remember", &full, json!({"text": "zk room b links [[zk target note]]"}), &[("x-dm-write-scope", "room:b")]).await;
        let mcp = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "reindex_links", "arguments": {}}});
        let tool_text = |v: &serde_json::Value| v["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        let edges = |app: Router, full: String, target: String| async move {
            post_as(&app, "/edges", &full, json!({"uri": target}), &[]).await.1.as_array().map(|a| a.len()).unwrap_or(0)
        };
        for (tok, hdrs, reason) in [
            (&bridge, &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "room:a")][..], "batch graph maintenance needs a full-tenant token writing global"),
            (&bridge, &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")][..], "batch graph maintenance needs a full-tenant token writing global"),
            (&bridge, &[][..], "this identity has no write scope"),
            (&full, &[][..], crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED),
        ] {
            let (st, v) = post_as(&app, "/mcp", tok, mcp.clone(), hdrs).await;
            assert_eq!((st, &v["result"]["isError"]), (StatusCode::OK, &json!(true)), "{v}");
            assert!(tool_text(&v).contains(reason), "{v}");
        }
        assert_eq!(edges(app.clone(), full.clone(), target.clone()).await, 0, "no edge was written");
        let (st, v) = post_as(&app, "/reindex_links", &full, json!({}), &[]).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some(crate::tools::EXPLICIT_WRITE_SCOPE_REQUIRED)), "{v}");
        let (st, _) = post_as(&app, "/reindex_links", &bridge, json!({}), &[("x-dm-scopes", "room:a"), ("x-dm-write-scope", "")]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "REST still refuses scope-bound tokens up front");
        // a full-tenant token writing global ('' named on this policy tenant) runs it
        let (st, v) = post_as(&app, "/mcp", &full, mcp.clone(), &global).await;
        assert_eq!((st, &v["result"]["isError"]), (StatusCode::OK, &json!(false)), "{v}");
        assert_eq!(edges(app.clone(), full.clone(), target.clone()).await, 1);
        let (st, _) = post_as(&app, "/reindex_links", &full, json!({}), &global).await;
        assert_eq!(st, StatusCode::OK);
        std::env::remove_var("DM_DATA_DIR");
    }

    // The vector-slot migration runs at start-up for every tenant that needs it, caching the
    // migrated handle; a tenant whose first open is still running holds up only itself.
    #[cfg(feature = "zvec")]
    #[test]
    fn pending_vector_indexes_migrate_at_start_up_and_block_only_their_own_tenant() {
        use crate::embedder::Embedder;
        use crate::store::MemoryStore;
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("dmmigr-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        // a 0.3.6-format tenant: a room row after a global row under one uri, and ONE vector for
        // the uri under the bare key, holding the room's text
        let vdir = crate::config::vector_dir("migr").unwrap();
        let uri = crate::entry::make_uri("resources/workspace", crate::entry::Kind::Runbook, "zm shared");
        {
            let s = crate::sqlite::SqliteStore::open(&crate::config::db_path("migr").unwrap()).unwrap();
            let mut e = crate::entry::Entry::new_now(uri.clone(), crate::entry::Kind::Runbook, "resources/workspace".into(),
                "zm shared".into(), "operator steps".into(), vec![], 60, uri.clone());
            s.put(&e).unwrap();
            e.scope = "room:a".into();
            e.body = "room draft".into();
            s.put(&e).unwrap();
            let idx = crate::zvec_index::ZvecIndex::open(&vdir).unwrap();
            idx.upsert_chunks(&uri, "", &crate::embedder::HashEmbedder::new().embed_chunks("room draft")).unwrap();
        }
        // a tenant with no vector index yet is not pending
        crate::sqlite::SqliteStore::open(&crate::config::db_path("fresh").unwrap()).unwrap();
        assert_eq!(crate::tools::tenants_pending_slot_migration().unwrap(), vec!["migr".to_string()]);

        let st = app_state(Arc::new(BearerAuth::from_env().unwrap()), None);
        migrate_pending_vector_indexes(&st);
        assert!(crate::tools::tenants_pending_slot_migration().unwrap().is_empty(), "migrated, marker written");
        let cached = st.slot_for("migr").lock().unwrap().clone().expect("the start-up pass cached the handle");
        assert!(Arc::ptr_eq(&cached, &st.memory_for("migr").unwrap()), "the first request reuses it: no second open");

        // a tenant whose first open is under way (its slot is held) does not hold up another
        let busy = st.slot_for("busy");
        let held = busy.lock().unwrap();
        let st2 = st.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(st2.memory_for("other").is_ok());
        });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(60)), Ok(true), "another tenant opens meanwhile");
        drop(held);
        assert!(st.memory_for("busy").is_ok());
        std::env::remove_var("DM_DATA_DIR");
    }

    // GET /capabilities: what the server supports, for the token that asks (401 without one).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn capabilities_route_reports_write_scope_support_per_token() {
        let _g = env_lock();
        let (_dir, iam, full, bridge) = scope_fixture("capsx");
        let alice = iam.mint_scoped_token("capsx", "alice", None, &["user:alice".to_string()], Some("user:alice"), false).unwrap();
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let get = |tok: Option<&str>| {
            let mut b = Request::builder().method("GET").uri("/capabilities");
            if let Some(t) = tok {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            b.body(Body::empty()).unwrap()
        };
        let resp = app.clone().oneshot(get(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        for (tok, honoured) in [(&full, true), (&bridge, true), (&alice, false)] {
            let resp = app.clone().oneshot(get(Some(tok))).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
            assert_eq!((&v["write_scope"], &v["write_scope_header"]), (&json!(true), &json!(honoured)), "{v}");
            assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        }
        std::env::remove_var("DM_DATA_DIR");
    }

    /// Serve `app` on a free loopback port on its own runtime thread: (base url, stop, join).
    #[cfg(feature = "client")]
    fn serve_on_loopback(app: Router) -> (String, std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let join = std::thread::spawn(move || {
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app)
                    .with_graceful_shutdown(async move {
                        let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
                    })
                    .await
                    .unwrap();
            })
        });
        (format!("http://{addr}"), stop_tx, join)
    }

    // A 0.3.7 client against an older server: `--scope` is refused BEFORE anything is sent
    // (the old server would ignore the header and store the write globally, or retract every
    // scope's rows on a forget), and `dmem admin policy` names the version problem.
    #[cfg(feature = "client")]
    #[test]
    fn client_refuses_scoped_writes_to_a_server_older_than_0_3_7() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let writes = Arc::new(AtomicUsize::new(0));
        // the pre-0.3.7 surface: saves and forget, no /capabilities, no /admin/tenant_policy
        let w = writes.clone();
        let w2 = writes.clone();
        let old = Router::new()
            .route("/remember", post(move || async move {
                w.fetch_add(1, Ordering::SeqCst);
                Json(json!({"uri": "daimon://resources/notes/memory/x"}))
            }))
            .route("/forget", post(move || async move {
                w2.fetch_add(1, Ordering::SeqCst);
                Json(json!({"forgotten": 2}))
            }));
        let (url, stop, join) = serve_on_loopback(old);
        let link = crate::config::ServerLink { url, token: "tok-0123456789abcdef".into(), insecure: false, ca_cert: None };
        let scoped = crate::client::RemoteClient::new(&link).unwrap().with_write_scope(Some("room:x".into())).unwrap();
        for e in [
            scoped.remember("a room note", "resources/notes", None, None).expect_err("refused"),
            scoped.forget("daimon://resources/notes/memory/x").expect_err("refused"),
        ] {
            assert!(format!("{e:#}").contains("older than 0.3.7"), "{e:#}");
        }
        assert_eq!(writes.load(Ordering::SeqCst), 0, "nothing reached the old server");
        // without --scope, the old server works as before
        let plain = crate::client::RemoteClient::new(&link).unwrap();
        plain.remember("a global note", "resources/notes", None, None).unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        let e = plain.admin_policy("workspace", Some(true)).expect_err("no such route on the old server");
        assert!(format!("{e:#}").contains("older than 0.3.7") && format!("{e:#}").contains("upgrade the server"), "{e:#}");
        let _ = stop.send(());
        join.join().unwrap();
    }

    // The save reply's scope must match `--scope`, or the client reports where the record went
    // instead of "stored" (a proxy that rewrote the header, a server that lost it).
    #[cfg(feature = "client")]
    #[test]
    fn client_reports_a_save_that_did_not_land_in_the_requested_scope() {
        let caps = || async { Json(json!({"version": "0.3.7", "write_scope": true, "write_scope_header": true})) };
        let app = Router::new()
            .route("/capabilities", get(caps))
            .route("/remember", post(|| async { Json(json!({"uri": "daimon://resources/notes/memory/x", "scope": ""})) }))
            .route("/log_lesson", post(|| async { Json(json!({"uri": "daimon://agent/lessons/agent_lesson/y"})) }));
        let (url, stop, join) = serve_on_loopback(app);
        let link = crate::config::ServerLink { url, token: "tok-0123456789abcdef".into(), insecure: false, ca_cert: None };
        let c = crate::client::RemoteClient::new(&link).unwrap().with_write_scope(Some("room:x".into())).unwrap();
        let e = c.remember("a room note", "resources/notes", None, None).expect_err("landed elsewhere");
        assert!(format!("{e:#}").contains("in scope '', not the requested 'room:x'"), "{e:#}");
        let e = c.log_lesson("t", "l", "agent/lessons").expect_err("unconfirmed");
        assert!(format!("{e:#}").contains("did not report the scope"), "{e:#}");
        let _ = stop.send(());
        join.join().unwrap();
    }

    // Against this server: a plain scope-bound token cannot be pointed at another scope with
    // `--scope` (the server would ignore it); nothing is written.
    #[cfg(feature = "client")]
    #[test]
    fn client_refuses_scope_for_a_scope_bound_token() {
        let _g = env_lock();
        let (dir, iam, _full, _bridge) = scope_fixture("boundc");
        let alice = iam.mint_scoped_token("boundc", "alice", None, &["user:alice".to_string()], Some("user:alice"), false).unwrap();
        let (url, stop, join) = serve_on_loopback(router(Arc::new(BearerAuth::from_env().unwrap()), Some(iam)));
        let link = crate::config::ServerLink { url, token: alice, insecure: false, ca_cert: None };
        let e = crate::client::RemoteClient::new(&link).unwrap().with_write_scope(Some("room:x".into())).unwrap()
            .remember("zq alice via scope", "resources/notes", None, None).expect_err("refused");
        assert!(format!("{e:#}").contains("bound to its own write scope"), "{e:#}");
        let uri = crate::client::RemoteClient::new(&link).unwrap().remember("zq alice plain", "resources/notes", None, None).unwrap();
        assert!(uri.starts_with("daimon://"));
        let _ = stop.send(());
        join.join().unwrap();
        assert!(live_rows_with_body(&dir, "boundc", "zq alice via scope").is_empty());
        assert_eq!(live_rows_with_body(&dir, "boundc", "zq alice plain"), vec!["user:alice"]);
        std::env::remove_var("DM_DATA_DIR");
    }

    #[test]
    fn bearer_resolves_tenant() {
        let _g = env_lock();
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
        let _g = env_lock();
        std::env::set_var("DM_TOKEN_WORKSPACE__ASSISTANT", "agtok1-0123456789abc"); // tenant + agent
        std::env::set_var("DM_TOKEN_MY_TENANT__ANALYST", "agtok2-0123456789abc"); // single underscores stay in the tenant
        std::env::set_var("DM_TOKEN_EDGE__", "agtok3-0123456789abc"); // trailing __ = agent-less
        std::env::set_var("DM_TOKEN___GHOST", "agtok4-0123456789abc"); // empty tenant = ignored
        std::env::set_var("DM_TOKEN_T2__A__B", "agtok5-0123456789abc"); // FIRST __ splits; the rest is the agent
        let a = BearerAuth::from_env().unwrap();

        let id = a.identity_for(Some("Bearer agtok1-0123456789abc")).expect("agent token resolves");
        assert_eq!(id.tenant.as_deref(), Some("workspace"));
        assert_eq!(id.agent.as_deref(), Some("assistant"), "env agent label is canonicalized (lowercase)");
        assert!(!id.is_admin);

        let id = a.identity_for(Some("Bearer agtok2-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("my_tenant"), "single underscores belong to the tenant");
        assert_eq!(id.agent.as_deref(), Some("analyst"));

        let id = a.identity_for(Some("Bearer agtok3-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("edge"));
        assert!(id.agent.is_none(), "trailing __ means agent-less");

        assert!(a.identity_for(Some("Bearer agtok4-0123456789abc")).is_none(), "empty tenant is skipped");

        let id = a.identity_for(Some("Bearer agtok5-0123456789abc")).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("t2"));
        assert_eq!(id.agent.as_deref(), Some("a__b"), "only the first __ splits");

        for k in ["DM_TOKEN_WORKSPACE__ASSISTANT", "DM_TOKEN_MY_TENANT__ANALYST", "DM_TOKEN_EDGE__", "DM_TOKEN___GHOST", "DM_TOKEN_T2__A__B"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn duplicate_secret_to_different_tenants_fails_fast() {
        let _g = env_lock();
        std::env::set_var("DM_TOKEN_ACME", "shared-0123456789abc");
        std::env::set_var("DM_TOKEN_GLOBEX", "shared-0123456789abc");
        let r = BearerAuth::from_env();
        assert!(r.is_err(), "same secret -> two tenants must be rejected");
        std::env::remove_var("DM_TOKEN_ACME");
        std::env::remove_var("DM_TOKEN_GLOBEX");
    }

    #[test]
    fn duplicate_secret_to_different_agents_fails_fast() {
        let _g = env_lock();
        std::env::set_var("DM_TOKEN_ACME2__ASSISTANT", "shared2-0123456789ab");
        std::env::set_var("DM_TOKEN_ACME2__DEVIN", "shared2-0123456789ab");
        let r = BearerAuth::from_env();
        assert!(r.is_err(), "same secret -> same tenant but two agents must be rejected (attribution)");
        std::env::remove_var("DM_TOKEN_ACME2__ASSISTANT");
        std::env::remove_var("DM_TOKEN_ACME2__DEVIN");
    }

    #[test]
    fn memory_cache_reuses_one_handle_per_tenant() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("dmcache-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        let st = app_state(Arc::new(BearerAuth::from_env().unwrap()), None);
        let a1 = st.memory_for("tenant_a").unwrap();
        let a2 = st.memory_for("tenant_a").unwrap();
        let b1 = st.memory_for("tenant_b").unwrap();
        assert!(Arc::ptr_eq(&a1, &a2), "same tenant must reuse the cached handle");
        assert!(!Arc::ptr_eq(&a1, &b1), "different tenants must get different handles");
        std::env::remove_var("DM_DATA_DIR");
    }

    #[tokio::test]
    async fn recall_route_authorizes_and_returns_hits() {
        let _g = env_lock();
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
        let _g = env_lock();
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
        let _g = env_lock();
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
        let _g = env_lock();
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
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("dmpers-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_PT1__ASSISTANT", "asttok-0123456789abc");
        std::env::set_var("DM_TOKEN_PT1", "plaintok-0123456789a");
        let m = Memory::open_tenant("pt1").unwrap();
        m.import_record(crate::entry::Kind::Persona, "agents/assistant/persona", "Assistant Persona", "I am the assistant").unwrap();
        m.import_record(crate::entry::Kind::Persona, "agents/analyst/persona", "Analyst Persona", "I am the analyst").unwrap();
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

        let s = call("asttok-0123456789abc").await;
        assert!(s.contains("Assistant Persona") && s.contains("House Rules"), "own persona + shared governance: {s}");
        assert!(!s.contains("Analyst Persona"), "another agent's persona must not leak: {s}");

        let s = call("plaintok-0123456789a").await;
        assert!(s.contains("Assistant Persona") && s.contains("Analyst Persona"), "agent-less token keeps legacy behaviour: {s}");

        std::env::remove_var("DM_TOKEN_PT1__ASSISTANT");
        std::env::remove_var("DM_TOKEN_PT1");
        std::env::remove_var("DM_DATA_DIR");
    }

    // Write attribution over the wire: a save through an agent token comes back from recall
    // with the author:<agent> tag stamped by the server (the client sends no attribution).
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the awaits, as above
    #[tokio::test]
    async fn remember_with_agent_token_stamps_author() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("dmattr-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_AT1__ANALYST", "analystok-0123456789");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/remember")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer analystok-0123456789")
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
                    .header("authorization", "Bearer analystok-0123456789")
                    .body(Body::from(r#"{"query":"report template docs","limit":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("author:analyst"), "recalled record carries the token's agent as author: {s}");

        std::env::remove_var("DM_TOKEN_AT1__ANALYST");
        std::env::remove_var("DM_DATA_DIR");
    }

    // The MCP streamable-HTTP transport on POST /mcp: the same bearer gate as every other route
    // (before any parsing), JSON-RPC replies as application/json, 202 for notifications, a
    // JSON-RPC parse error for a malformed body, 405 for GET, and a tool surface identical to
    // stdio (dispatch goes through mcp::handle, never a copy of it).
    #[allow(clippy::await_holding_lock)] // ENV_LOCK must span the awaits, as above
    #[tokio::test]
    async fn mcp_route_speaks_json_rpc_over_http() {
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("dmmcph-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::env::set_var("DM_DATA_DIR", &dir);
        std::env::set_var("DM_TOKEN_MCP1", "mcptok-0123456789abcd");
        let app = router(Arc::new(BearerAuth::from_env().unwrap()), None);

        let post = |body: &'static str, auth: bool| {
            let mut b = Request::builder().method("POST").uri("/mcp").header("content-type", "application/json");
            if auth {
                b = b.header("authorization", "Bearer mcptok-0123456789abcd");
            }
            b.body(Body::from(body)).unwrap()
        };
        let call = |app: Router, req: Request<Body>| async move {
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            (status, ctype, bytes)
        };

        // (1) no bearer -> 401 before any JSON-RPC parsing (a malformed body changes nothing)
        let (st, _, _) = call(app.clone(), post("{not json", false)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _, _) = call(app.clone(), post(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, false)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);

        // GET -> 405 (no server-initiated messages)
        let get = Request::builder()
            .method("GET")
            .uri("/mcp")
            .header("authorization", "Bearer mcptok-0123456789abcd")
            .body(Body::empty())
            .unwrap();
        let (st, _, _) = call(app.clone(), get).await;
        assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);

        // (2) tools/list -> 200 application/json with exactly the stdio tool schemas
        let (st, ctype, bytes) = call(app.clone(), post(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, true)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(ctype.starts_with("application/json"), "content-type: {ctype}");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["tools"], crate::mcp::tool_schemas(), "HTTP tool surface must equal stdio");

        // (3) a notification -> 202 with an empty body
        let (st, _, bytes) = call(app.clone(), post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, true)).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(bytes.is_empty(), "notification reply must carry no body: {}", String::from_utf8_lossy(&bytes));

        // (4) malformed JSON -> 400 with a JSON-RPC parse error (null id)
        let (st, ctype, bytes) = call(app.clone(), post("{not json", true)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(ctype.starts_with("application/json"), "content-type: {ctype}");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["id"].is_null());
        // a well-formed but non-object body is an invalid request, not a silent 202
        let (st, _, bytes) = call(app.clone(), post("[1,2,3]", true)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], -32600);

        // initialize negotiates the streamable-HTTP protocol revision, stateless (no session header)
        let (st, _, bytes) = call(
            app.clone(),
            post(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#, true),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "dmem");

        // a tool call writes into the tenant store and reads back through the same transport
        let (st, _, bytes) = call(
            app.clone(),
            post(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remember","arguments":{"text":"the backup cron runs restic nightly","namespace":"resources/notes"}}}"#, true),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"]["isError"], false, "{v}");
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("stored daimon://"), "{v}");
        let (st, _, bytes) = call(
            app.clone(),
            post(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall","arguments":{"query":"restic backup cron","limit":5}}}"#, true),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("restic"), "recall over MCP finds the save: {v}");

        std::env::remove_var("DM_TOKEN_MCP1");
        std::env::remove_var("DM_DATA_DIR");
    }

    /// A test embedder: the hash embedder, but a text containing `SLOW_MARK` takes `delay` to
    /// embed (a long body on a real model); `started` flips when such an embed begins.
    #[cfg(feature = "zvec")]
    struct SlowEmbedder {
        inner: crate::embedder::HashEmbedder,
        delay: std::time::Duration,
        started: std::sync::atomic::AtomicBool,
        /// Slow embeds running now, and the most that ever ran at once.
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    #[cfg(feature = "zvec")]
    impl SlowEmbedder {
        fn new(delay: std::time::Duration) -> Self {
            Self {
                inner: crate::embedder::HashEmbedder::new(),
                delay,
                started: AtomicBool::new(false),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }
    }

    #[cfg(feature = "zvec")]
    const SLOW_MARK: &str = "zslowembed";

    #[cfg(feature = "zvec")]
    impl crate::embedder::Embedder for SlowEmbedder {
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn name(&self) -> &'static str {
            // the hash embedder's cosine scale (the recall floor keys on the name)
            self.inner.name()
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            if text.contains(SLOW_MARK) {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                self.started.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(self.delay);
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            self.inner.embed(text)
        }
    }

    // P0-5: a save whose embedding is slow (a 64-window body took 93.6 s on a small server)
    // does not hold the tenant lock while the model runs, so a recall on the SAME tenant is
    // answered meanwhile. Before, it waited for the whole save.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_long_save_does_not_block_a_same_tenant_recall() {
        let _g = env_lock();
        let (_dir, iam, full, _bridge) = scope_fixture("lockx");
        let slow = Arc::new(SlowEmbedder::new(std::time::Duration::from_secs(4)));
        let mut st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        st.embedder = slow.clone();
        let app = router_with_state(st.clone());
        let (code, _) = post_as(&app, "/remember", &full, json!({"text": "zlk the ceph pool is healthy"}), &[]).await;
        assert_eq!(code, StatusCode::OK);

        let long_text = format!("zlk long incident write-up {SLOW_MARK}");
        let (app2, full2) = (app.clone(), full.clone());
        let save = tokio::spawn(async move { post_as(&app2, "/remember", &full2, json!({"text": long_text}), &[]).await });
        let t0 = std::time::Instant::now();
        while !slow.started.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "the slow embed never started");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let t = std::time::Instant::now();
        let (code, v) = post_as(&app, "/recall", &full, json!({"query": "ceph pool", "limit": 5}), &[]).await;
        let took = t.elapsed();
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(v.as_array().is_some_and(|a| a.iter().any(|h| h["body"] == "zlk the ceph pool is healthy")), "{v}");
        assert!(took < std::time::Duration::from_millis(1500), "a same-tenant recall waited {took:?} behind the save's embed");
        assert!(!save.is_finished(), "the save was still embedding while the recall was answered");
        let (code, v) = save.await.unwrap();
        assert_eq!(code, StatusCode::OK, "{v}");
        // the save's vector landed (hybrid recall finds it), and it was embedded once, before the
        // lock: the tenant handle computed no embedding itself
        let (_, v) = post_as(&app, "/recall", &full, json!({"query": "zlk long incident write-up", "limit": 5}), &[]).await;
        assert!(v.as_array().is_some_and(|a| a.iter().any(|h| h["body"].as_str().is_some_and(|b| b.contains(SLOW_MARK)))), "{v}");
        let handle = st.memory_for("lockx").unwrap();
        let n = local_ref(&handle.lock().unwrap()).engine_embed_count();
        assert_eq!(n, 0, "nothing was embedded under the tenant lock");
        std::env::remove_var("DM_DATA_DIR");
    }

    // P0-5: every route that embeds plans its texts exactly (the plan builds the body the save
    // stores, the query the recall runs), so no embedding runs under the tenant lock: REST
    // saves and recalls, and the MCP tools and prompt.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn every_embedding_route_embeds_before_the_tenant_lock() {
        let _g = env_lock();
        let (_dir, iam, full, _bridge) = scope_fixture("planx");
        let mut st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        st.embedder = Arc::new(crate::embedder::HashEmbedder::new());
        let app = router_with_state(st.clone());
        let rest: Vec<(&str, serde_json::Value)> = vec![
            ("/remember", json!({"text": "zp remember body", "valid_from": 1000})),
            ("/log_decision", json!({"title": "zp d", "context": "c", "decision": "d", "rationale": "r"})),
            ("/log_lesson", json!({"title": "zp l", "lesson": "l"})),
            ("/log_incident", json!({"title": "zp i", "impact": "i", "resolution": "r"})),
            ("/log_runbook", json!({"title": "zp rb", "steps": "s"})),
            ("/log_convention", json!({"title": "zp c", "rule": "r"})),
            ("/add_reminder", json!({"title": "zp rem", "text": "t"})),
            ("/import", json!({"kind": "runbook", "title": "zp imp", "body": "zp imported body"})),
            ("/import", json!({"kind": "runbook", "title": "zp imp2", "body": "zp migrated body", "created_ms": 5000})),
            ("/recall", json!({"query": "zp remember"})),
            ("/recall", json!({"query": "zp remember", "namespaces": ["resources/"]})),
            ("/recall_expanded", json!({"query": "zp decision"})),
        ];
        for (path, body) in rest {
            let (code, v) = post_as(&app, path, &full, body.clone(), &[]).await;
            assert_eq!(code, StatusCode::OK, "{path} {body}: {v}");
        }
        let calls = [
            json!({"name": "remember", "arguments": {"text": "zp mcp note"}}),
            json!({"name": "remember", "arguments": {"text": "zp mcp typed", "kind": "runbook", "title": "zp mt"}}),
            json!({"name": "log_decision", "arguments": {"title": "zp md", "decision": "d"}}),
            json!({"name": "log_lesson", "arguments": {"title": "zp ml", "lesson": "l"}}),
            json!({"name": "log_incident", "arguments": {"title": "zp mi", "impact": "i"}}),
            json!({"name": "add_reminder", "arguments": {"title": "zp mr", "text": "t"}}),
            json!({"name": "entity", "arguments": {"kind": "org", "name": "zp Org", "attrs": {"tier": "gold", "seats": 40}, "desc": "d"}}),
            json!({"name": "recall", "arguments": {"query": "zp mcp", "expand": 0}}),
            json!({"name": "recall", "arguments": {"query": "zp mcp"}}),
            // a null as_of is no as-of slice: a vector recall, planned like any other
            json!({"name": "recall", "arguments": {"query": "zp mcp", "as_of": null}}),
        ];
        for (i, params) in calls.iter().enumerate() {
            let req = json!({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": params});
            let (code, v) = post_as(&app, "/mcp", &full, req, &[]).await;
            assert_eq!((code, &v["result"]["isError"]), (StatusCode::OK, &json!(false)), "{params}: {v}");
        }
        let req = json!({"jsonrpc": "2.0", "id": 99, "method": "prompts/get", "params": {"name": "recall", "arguments": {"query": "  zp mcp  "}}});
        let (code, v) = post_as(&app, "/mcp", &full, req, &[]).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let handle = st.memory_for("planx").unwrap();
        let n = local_ref(&handle.lock().unwrap()).engine_embed_count();
        assert_eq!(n, 0, "{n} embedding(s) ran under the tenant lock: a route's plan does not match what it embeds");
        std::env::remove_var("DM_DATA_DIR");
    }

    /// A router whose `/slow` handler marks `started`, sleeps `hold`, then answers "done".
    fn slow_router(started: Arc<AtomicBool>, hold: Duration) -> Router {
        Router::new().route(
            "/slow",
            get(move || {
                let started = started.clone();
                async move {
                    started.store(true, Ordering::SeqCst);
                    tokio::time::sleep(hold).await;
                    "done"
                }
            }),
        )
    }

    /// GET `path` over plain HTTP/1.1 on a std socket: (status, body).
    fn get_plain(addr: std::net::SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
        use std::io::{Read, Write};
        let mut c = std::net::TcpStream::connect(addr)?;
        c.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())?;
        let mut raw = String::new();
        c.read_to_string(&mut raw)?;
        let code = raw.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        Ok((code, raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()))
    }

    /// Wait (up to 5 s) until a fresh connection to `addr` is refused: the listener is gone.
    fn wait_refused(addr: std::net::SocketAddr) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if std::net::TcpStream::connect(addr).is_err() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    // P0-2, plain HTTP: after the stop signal the server accepts no new connection, lets the
    // request already in flight finish (200), and only then returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_stop_drains_in_flight_requests_and_refuses_new_ones() {
        let started = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_plain(
            listener,
            slow_router(started.clone(), Duration::from_millis(1500)),
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_secs(20),
        ));
        let client = std::thread::spawn(move || get_plain(addr, "/slow"));
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let t = Instant::now();
        stop_tx.send(()).unwrap();
        assert!(tokio::task::spawn_blocking(move || wait_refused(addr)).await.unwrap(), "still accepting after the stop");
        assert!(!server.is_finished(), "returned before the in-flight request finished");
        let (code, body) = tokio::task::spawn_blocking(move || client.join().unwrap()).await.unwrap().unwrap();
        assert_eq!((code, body.as_str()), (200, "done"), "the in-flight request completed");
        server.await.unwrap().unwrap();
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    // P0-2: the drain is bounded: a request still running when the grace runs out is cut off
    // and the server returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_gives_up_on_requests_after_the_grace() {
        let started = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_plain(
            listener,
            slow_router(started.clone(), Duration::from_secs(60)),
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_millis(300),
        ));
        let _client = std::thread::spawn(move || get_plain(addr, "/slow"));
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let t = Instant::now();
        stop_tx.send(()).unwrap();
        server.await.unwrap().unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "waited {:?} for a 60 s request", t.elapsed());
    }

    // P0-2, TLS (it had no stop handling at all): the same drain over HTTPS.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_stop_drains_in_flight_requests_and_refuses_new_ones() {
        crate::tls::install_provider();
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (cert_pem, key_pem) = (ck.cert.pem(), ck.key_pair.serialize_pem());
        let cfg = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem.clone().into_bytes(), key_pem.into_bytes()).await.unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let sock: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_tls(
            sock,
            cfg,
            slow_router(started.clone(), Duration::from_millis(1500)),
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_secs(20),
        ));
        let client = std::thread::spawn(move || {
            let ca = reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap();
            // the client dmem ships (rustls, HTTP/1.1), not whatever TLS reqwest picks in this build
            let c = crate::tls::http_builder().add_root_certificate(ca).build().unwrap();
            // retry until the listener is up
            for _ in 0..100 {
                match c.get(format!("https://localhost:{port}/slow")).send() {
                    Ok(r) => return (r.status().as_u16(), r.text().unwrap_or_default()),
                    Err(e) if e.is_connect() => std::thread::sleep(Duration::from_millis(50)),
                    Err(e) => panic!("{e}"),
                }
            }
            panic!("never connected");
        });
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop_tx.send(()).unwrap();
        assert!(tokio::task::spawn_blocking(move || wait_refused(sock)).await.unwrap(), "still accepting after the stop");
        assert!(!server.is_finished(), "returned before the in-flight request finished");
        let (code, body) = tokio::task::spawn_blocking(move || client.join().unwrap()).await.unwrap();
        assert_eq!((code, body.as_str()), (200, "done"));
        server.await.unwrap().unwrap();
    }

    // Key exchange on the server's own TLS config: a client that offers only the post-quantum
    // hybrid negotiates X25519MLKEM768, one without ML-KEM (dmem 0.3.7, an older browser) still
    // connects over X25519, and a client that prefers the hybrid (dmem 0.3.8) gets it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_negotiates_the_pq_hybrid_and_falls_back_to_x25519() {
        use rustls::crypto::aws_lc_rs::{default_provider, kx_group};
        use rustls::NamedGroup;
        use std::io::{Read, Write};
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir().join(format!("dmtlskx-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, ck.cert.pem()).unwrap();
        std::fs::write(&key, ck.key_pair.serialize_pem()).unwrap();
        let opts = TlsOpts {
            cert: Some(cert.display().to_string()),
            key: Some(key.display().to_string()),
            generate: false,
            sans: vec![],
        };
        let cfg = tls_config("127.0.0.1:0", &opts).await.unwrap().expect("cert and key given");
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let sock: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().route("/", get(|| async { "ok" }));
        let server = tokio::spawn(serve_tls(
            sock,
            cfg,
            app,
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_secs(1),
        ));
        let root = ck.cert.der().clone();
        let negotiated = tokio::task::spawn_blocking(move || {
            let offers = [vec![kx_group::X25519MLKEM768], vec![kx_group::X25519], default_provider().kx_groups];
            offers.map(|kx_groups| {
                let mut roots = rustls::RootCertStore::empty();
                roots.add(root.clone()).unwrap();
                let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::CryptoProvider {
                    kx_groups,
                    ..default_provider()
                }))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
                let conn = rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap();
                // retry until the listener is up
                let tcp = (0..100)
                    .find_map(|_| {
                        std::net::TcpStream::connect(sock).ok().or_else(|| {
                            std::thread::sleep(Duration::from_millis(50));
                            None
                        })
                    })
                    .expect("never connected");
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
                let mut status = [0u8; 12];
                tls.read_exact(&mut status).unwrap();
                assert_eq!(&status, b"HTTP/1.1 200");
                tls.conn.negotiated_key_exchange_group().map(|g| g.name())
            })
        })
        .await
        .unwrap();
        assert_eq!(
            negotiated,
            [Some(NamedGroup::X25519MLKEM768), Some(NamedGroup::X25519), Some(NamedGroup::X25519MLKEM768)]
        );
        stop_tx.send(()).unwrap();
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_grace_parses_seconds_with_a_safe_default() {
        assert_eq!(parse_shutdown_grace(None), Duration::from_secs(15));
        assert_eq!(parse_shutdown_grace(Some("")), Duration::from_secs(15));
        assert_eq!(parse_shutdown_grace(Some(" 45 ")), Duration::from_secs(45));
        assert_eq!(parse_shutdown_grace(Some("0")), Duration::ZERO);
        for bad in ["-1", "ten", "3601", "1.5"] {
            assert_eq!(parse_shutdown_grace(Some(bad)), Duration::from_secs(15), "{bad}");
        }
        // the worst case after SIGTERM stays inside the default 30 s termination grace with a 5 s
        // pre-stop: grace + TLS drain backup + close budget + runtime shutdown, plus 3 s to flush
        let worst = DEFAULT_SHUTDOWN_GRACE + Duration::from_secs(1) + CLOSE_BUDGET + Duration::from_secs(1);
        assert!(worst + Duration::from_secs(3) <= Duration::from_secs(30 - 5), "{worst:?}");
    }

    // P0-2: after the drain every cached tenant's vector index is flushed and the tenant
    // closed; a tenant still in its first open is left to its (resumable) migration, one a
    // request still holds is skipped at the deadline, and nothing opens a tenant or takes a
    // tenant lock afterwards (503).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_tenants_flushes_every_open_tenant_and_refuses_new_work() {
        let _g = env_lock();
        let (_dir, iam, full, _bridge) = scope_fixture("closex");
        let st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        let app = router_with_state(st.clone());
        let (code, _) = post_as(&app, "/remember", &full, json!({"text": "zcl flushed at shutdown"}), &[]).await;
        assert_eq!(code, StatusCode::OK);
        st.memory_for("closey").unwrap();
        // a tenant mid-open (its slot held) and one whose lock a request holds
        let opening = st.slot_for("closeopening");
        let busy = st.memory_for("closebusy").unwrap();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _g = busy.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = hold_rx.recv_timeout(Duration::from_secs(30));
        });
        held_rx.recv().unwrap();
        let st2 = st.clone();
        let report = tokio::task::spawn_blocking(move || {
            let _slot = opening.lock().unwrap();
            st2.close_tenants(Instant::now() + Duration::from_millis(300))
        })
        .await
        .unwrap();
        let _ = hold_tx.send(());
        holder.join().unwrap();
        #[cfg(feature = "zvec")]
        assert_eq!(report.flushed, 2, "{report:?}");
        assert_eq!(report.closed, 2, "{report:?}");
        assert_eq!(report.opening, vec!["closeopening".to_string()]);
        assert_eq!(report.busy, vec!["closebusy".to_string()]);
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(st.slot_for("closex").lock().unwrap().is_none(), "the cache let go of the closed tenant");
        let e = st.memory_for("closex").err().expect("no tenant opens after close");
        assert!(e.downcast_ref::<ShuttingDown>().is_some(), "{e:#}");
        let (code, v) = post_as(&app, "/recall", &full, json!({"query": "zcl"}), &[]).await;
        assert_eq!((code, v["error"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("server is shutting down")));
        std::env::remove_var("DM_DATA_DIR");
    }

    /// The hash embedder under a model's name: readiness takes it for a real model.
    #[cfg(feature = "zvec")]
    struct NamedEmbedder(crate::embedder::HashEmbedder);

    #[cfg(feature = "zvec")]
    impl crate::embedder::Embedder for NamedEmbedder {
        fn dim(&self) -> usize {
            self.0.dim()
        }
        fn name(&self) -> &'static str {
            "test-model"
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.0.embed(text)
        }
    }

    async fn get_as(app: &Router, uri: &str, tok: Option<&str>) -> (StatusCode, String, Option<String>) {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned(), ctype)
    }

    /// `/readyz`: (status, failing checks, body).
    #[cfg(feature = "zvec")]
    async fn readyz(app: &Router) -> (StatusCode, Vec<String>, serde_json::Value) {
        let (code, body, _) = get_as(app, "/readyz", None).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let failing = v["failing"].as_array().map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()).unwrap_or_default();
        (code, failing, v)
    }

    // P0-3: /readyz is 200 only when the server should take traffic, and names each failing
    // check (IAM missing, placeholder embedder, a tenant without a vector index, a data dir it
    // cannot write, a stop in progress) without naming tenants. /healthz stays a static 200.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn readyz_fails_on_each_condition_and_healthz_stays_static() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("readyx");
        let admin = iam.ensure_admin_from(None).unwrap().expect("bootstrap admin");
        let mut st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        st.embedder = Arc::new(NamedEmbedder(crate::embedder::HashEmbedder::new()));
        st.io_check_ttl = Duration::ZERO;
        let app = router_with_state(st.clone());
        let (code, failing, v) = readyz(&app).await;
        assert_eq!((code, failing.len()), (StatusCode::OK, 0), "{v}");
        assert_eq!(v["status"], "ready");
        assert!(v["checks"].as_object().unwrap().values().all(|c| c == "pass"), "no detail without a token: {v}");

        // the start-up pass still running
        st.started.store(false, Ordering::SeqCst);
        assert_eq!(readyz(&app).await.1, vec!["startup".to_string()]);
        st.started.store(true, Ordering::SeqCst);
        st.memory_for("readyx").unwrap();
        assert_eq!(readyz(&app).await.0, StatusCode::OK, "an opened tenant with its index");

        // a tenant whose vector index did not open (a file where its directory belongs)
        std::fs::write(dir.join("vectors").join("brokenvec"), "not an index").unwrap();
        st.memory_for("brokenvec").unwrap();
        let (code, failing, v) = readyz(&app).await;
        assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["vectors".to_string()]), "{v}");
        assert!(!v.to_string().contains("brokenvec"), "no tenant names in the body: {v}");
        st.vectors.lock().unwrap().remove("brokenvec");

        // the placeholder embedder
        let mut hashed = st.clone();
        hashed.embedder = Arc::new(crate::embedder::HashEmbedder::new());
        let hashed = router_with_state(hashed);
        let (code, failing, v) = readyz(&hashed).await;
        assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["embedder".to_string()]));
        // without a token the body says which check fails, not why; the admin (or metrics)
        // token gets the detail
        assert_eq!(v["checks"]["embedder"], "fail", "{v}");
        assert!(!v.to_string().contains("placeholder") && !v.to_string().contains("hash"), "{v}");
        for tok in [None, Some(full.as_str()), Some("not-a-token-0123456789")] {
            let (_, body, _) = get_as(&hashed, "/readyz", tok).await;
            assert!(!body.contains("placeholder"), "{tok:?}: {body}");
        }
        let (code, body, _) = get_as(&hashed, "/readyz", Some(&admin)).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("placeholder hash embedder"), "the admin token sees why: {body}");
        assert!(body.contains("degraded: placeholder embedder"), "{body}");

        // no IAM (env-only serving), and an IAM whose file is gone
        let mut no_iam = st.clone();
        no_iam.iam = Arc::new(Mutex::new(None));
        let (code, failing, _) = readyz(&router_with_state(no_iam)).await;
        assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["iam".to_string()]));
        let moved = dir.join("iam.db.moved");
        std::fs::rename(dir.join("iam.db"), &moved).unwrap();
        assert_eq!(readyz(&app).await.1, vec!["iam".to_string()]);
        std::fs::rename(&moved, dir.join("iam.db")).unwrap();
        assert_eq!(readyz(&app).await.0, StatusCode::OK);

        // a data dir it cannot write (skipped as root, which writes anyway)
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("tenants"), std::fs::Permissions::from_mode(0o500)).unwrap();
            let (code, failing, _) = readyz(&app).await;
            std::fs::set_permissions(dir.join("tenants"), std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["data_dir".to_string()]));
        }

        // a stop in progress
        st.draining.store(true, Ordering::SeqCst);
        let (code, failing, _) = readyz(&app).await;
        assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["shutdown".to_string()]));
        // /healthz is liveness only, whatever readiness says
        let (code, body, _) = get_as(&app, "/healthz", None).await;
        assert_eq!((code, body.as_str()), (StatusCode::OK, r#"{"status":"ok"}"#));
        std::env::remove_var("DM_DATA_DIR");
    }

    // P0-3: /metrics answers the admin token or DM_METRICS_TOKEN (401 without a token, 403 for a
    // tenant token) in the Prometheus text format, with the lock, latency, embedder and vector
    // index series, and no tenant name anywhere.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metrics_needs_an_admin_or_scrape_token_and_reports_the_series() {
        let _g = env_lock();
        let (_dir, iam, full, _bridge) = scope_fixture("metricsx");
        // not ensure_admin(): that reads the developer's own DM_ADMIN_TOKEN
        let admin = iam.ensure_admin_from(None).unwrap().expect("bootstrap admin");
        let scrape = "scrape-secret-0123456789";
        std::env::set_var("DM_METRICS_TOKEN", scrape);
        let st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        std::env::remove_var("DM_METRICS_TOKEN");
        let app = router_with_state(st.clone());
        // the embedding series are process-wide: count this test's own requests
        let before_lock = crate::metrics::embed_count(crate::metrics::EmbedPhase::BeforeLock);
        let (code, _) = post_as(&app, "/remember", &full, json!({"text": "zmx metrics probe note"}), &[]).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _) = post_as(&app, "/recall", &full, json!({"query": "zmx metrics"}), &[]).await;
        assert_eq!(code, StatusCode::OK);
        #[cfg(feature = "zvec")]
        assert!(
            crate::metrics::embed_count(crate::metrics::EmbedPhase::BeforeLock) >= before_lock + 2,
            "the save and the recall each embedded before the lock"
        );
        #[cfg(not(feature = "zvec"))]
        let _ = before_lock;

        assert_eq!(get_as(&app, "/metrics", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(get_as(&app, "/metrics", Some(&full)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(get_as(&app, "/metrics", Some("not-the-scrape-token-000")).await.0, StatusCode::UNAUTHORIZED);
        let (code, _, _) = get_as(&app, "/metrics", Some(&admin)).await;
        assert_eq!(code, StatusCode::OK);
        let (code, body, ctype) = get_as(&app, "/metrics", Some(scrape)).await;
        assert_eq!(code, StatusCode::OK);
        assert!(ctype.unwrap_or_default().starts_with("text/plain; version=0.0.4"));
        for series in [
            "dmem_build_info{version=",
            "dmem_ready ",
            "dmem_requests_in_flight ",
            "dmem_tenants_open 1",
            "dmem_tenant_lock_wait_seconds_count ",
            "dmem_tenant_lock_hold_seconds_count ",
            "dmem_request_duration_seconds_count{route=\"/recall\"}",
            "dmem_responses_total{route=\"/remember\",class=\"2xx\"}",
            "dmem_embedder_info{name=",
            "dmem_zvec_failures_total{op=\"upsert\"}",
            "dmem_embed_seconds_count{phase=\"under_lock\"}",
        ] {
            assert!(body.contains(series), "missing {series}:\n{body}");
        }
        #[cfg(feature = "zvec")]
        for series in ["dmem_embed_permits{pool=\"save\"} ", "dmem_embed_permit_wait_seconds_count{pool=\"query\"} "] {
            assert!(body.contains(series), "missing {series}:\n{body}");
        }
        assert!(!body.contains("metricsx"), "no tenant name in the metrics");
        std::env::remove_var("DM_DATA_DIR");
    }

    #[test]
    fn a_short_metrics_token_is_refused() {
        let _g = env_lock();
        std::env::set_var("DM_METRICS_TOKEN", "short");
        assert!(metrics_token_from_env().is_err());
        std::env::set_var("DM_METRICS_TOKEN", "");
        assert!(metrics_token_from_env().unwrap().is_none());
        std::env::set_var("DM_METRICS_TOKEN", "long-enough-0123456789");
        assert!(metrics_token_from_env().unwrap().is_some());
        std::env::remove_var("DM_METRICS_TOKEN");
    }

    /// A state for the readiness tests: a model-named embedder (so only what a test breaks
    /// fails), disk checks never reused, and vector retries due at once.
    #[cfg(feature = "zvec")]
    fn ready_state(iam: crate::iam::Iam) -> AppState {
        let mut st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        st.embedder = Arc::new(NamedEmbedder(crate::embedder::HashEmbedder::new()));
        st.io_check_ttl = Duration::ZERO;
        st.vector_retry_base = Duration::from_millis(1);
        st
    }

    /// Put a file where `tenant`'s vector index directory belongs: its index cannot open.
    #[cfg(feature = "zvec")]
    fn block_vector_index(dir: &std::path::Path, tenant: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir.join("vectors")).unwrap();
        let p = dir.join("vectors").join(tenant);
        std::fs::write(&p, "in the way").unwrap();
        p
    }

    // A tenant whose vector index did not open is retried on a schedule, so readiness comes
    // back once the cause is gone, with no restart and no request to that tenant (a server
    // that is not ready gets none). A retry that still fails waits twice as long next time.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tenant_without_its_vector_index_heals_once_the_cause_clears() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("healx");
        let st = ready_state(iam);
        let app = router_with_state(st.clone());
        let blocker = block_vector_index(&dir, "healx");
        let (code, v) = post_as(&app, "/remember", &full, json!({"text": "zhl saved while keyword only"}), &[]).await;
        assert_eq!(code, StatusCode::OK, "keyword-only, as before: {v}");
        let early = v["uri"].as_str().unwrap().to_string();
        assert_eq!(readyz(&app).await.1, vec!["vectors".to_string()]);

        // a retry while the cause persists changes nothing, and backs off
        tokio::time::sleep(Duration::from_millis(5)).await;
        let first = st.vector_retry.lock().unwrap()["healx"].delay;
        assert_eq!(tokio::task::spawn_blocking({ let st = st.clone(); move || st.retry_vector_indexes() }).await.unwrap(), 0);
        assert_eq!(st.vector_retry.lock().unwrap()["healx"].delay, first * 2, "the wait doubles");
        assert_eq!(readyz(&app).await.1, vec!["vectors".to_string()]);

        // the cause goes away: the next retry opens the index and readiness returns
        std::fs::remove_file(&blocker).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(tokio::task::spawn_blocking({ let st = st.clone(); move || st.retry_vector_indexes() }).await.unwrap(), 1);
        let (code, failing, v) = readyz(&app).await;
        assert_eq!((code, failing.len()), (StatusCode::OK, 0), "{v}");
        assert!(st.vector_retry.lock().unwrap().is_empty());
        let handle = st.memory_for("healx").unwrap();
        let index = local_ref(&handle.lock().unwrap()).vector_index().expect("the cache holds the new handle");
        // hybrid recall gates on cosine: the save made while keyword-only was re-embedded after
        // the heal (it has its vector), and new saves are hybrid again
        let has_vector = |uri: &str, body: &str| {
            index.search(&st.embedder.embed(body), 10).unwrap().iter().any(|h| h.uri == uri && h.score > 0.99)
        };
        assert!(has_vector(&early, "zhl saved while keyword only"), "re-embedded after the heal");
        let (code, v) = post_as(&app, "/remember", &full, json!({"text": "zhl saved after the heal"}), &[]).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(has_vector(v["uri"].as_str().unwrap(), "zhl saved after the heal"));
        std::env::remove_var("DM_DATA_DIR");
    }

    // Under DM_REQUIRE_VECTORS a tenant whose index cannot open does not open at all; the retry
    // schedule opens it once the cause is gone, again without a request to it.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_tenant_opens_on_retry_under_require_vectors() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("reqhealx");
        let st = ready_state(iam);
        let app = router_with_state(st.clone());
        let blocker = block_vector_index(&dir, "reqhealx");
        std::env::set_var("DM_REQUIRE_VECTORS", "1");
        let (code, _) = post_as(&app, "/recall", &full, json!({"query": "zrh"}), &[]).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "refused, not degraded");
        assert_eq!(readyz(&app).await.1, vec!["vectors".to_string()]);
        assert!(st.slot_for("reqhealx").lock().unwrap().is_none());
        std::fs::remove_file(&blocker).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let healed = tokio::task::spawn_blocking({ let st = st.clone(); move || st.retry_vector_indexes() }).await.unwrap();
        std::env::remove_var("DM_REQUIRE_VECTORS");
        assert_eq!(healed, 1);
        assert_eq!(readyz(&app).await.0, StatusCode::OK);
        assert!(st.slot_for("reqhealx").lock().unwrap().is_some(), "opened by the retry");
        let (code, v) = post_as(&app, "/remember", &full, json!({"text": "zrh stored after the retry"}), &[]).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        std::env::remove_var("DM_DATA_DIR");
    }

    #[test]
    fn vector_retries_back_off_from_5_s_to_5_min() {
        assert_eq!(next_retry_delay(VECTOR_RETRY_BASE, VECTOR_RETRY_BASE), Duration::from_secs(10));
        assert_eq!(next_retry_delay(Duration::ZERO, VECTOR_RETRY_BASE), VECTOR_RETRY_BASE);
        assert_eq!(next_retry_delay(Duration::from_secs(200), VECTOR_RETRY_BASE), VECTOR_RETRY_MAX);
        assert_eq!(next_retry_delay(VECTOR_RETRY_MAX, VECTOR_RETRY_BASE), VECTOR_RETRY_MAX);
    }

    // The start-up pass opens every known tenant that has a database (IAM's active tenants and
    // the env tokens' tenants) before the server reports ready, so a tenant whose vector index
    // cannot open is caught before traffic, not at its first request. A registered tenant never
    // written to is not opened (nothing to judge, no files created).
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn start_up_opens_known_tenants_before_the_server_is_ready() {
        let _g = env_lock();
        let (dir, iam, _full, _bridge) = scope_fixture("bootx");
        iam.create_tenant("idlex", "", "", None).unwrap();
        std::fs::create_dir_all(dir.join("tenants")).unwrap();
        for t in ["bootx", "bootenvx"] {
            crate::sqlite::SqliteStore::open(&dir.join("tenants").join(format!("{t}.db"))).unwrap();
        }
        block_vector_index(&dir, "bootenvx");
        std::env::set_var("DM_TOKEN_BOOTENVX", "bootenv-secret-0123456789");
        let st = ready_state(iam);
        std::env::remove_var("DM_TOKEN_BOOTENVX");
        let app = router_with_state(st.clone());
        st.started.store(false, Ordering::SeqCst);
        assert_eq!(readyz(&app).await.1, vec!["startup".to_string()]);
        let opened = tokio::task::spawn_blocking({ let st = st.clone(); move || st.open_known_tenants() }).await.unwrap();
        st.started.store(true, Ordering::SeqCst);
        assert_eq!(opened, 2, "bootx (IAM) and bootenvx (env token)");
        assert_eq!(readyz(&app).await.1, vec!["vectors".to_string()], "judged before any request");
        assert!(!dir.join("tenants").join("idlex.db").exists(), "a tenant never written to is not opened");
        std::env::remove_var("DM_DATA_DIR");
    }

    // The disk checks behind /readyz (a probe file written and synced, an IAM query) are reused
    // for a moment, so a probe loop cannot become an fsync loop; the probe leaves no file.
    #[cfg(all(unix, feature = "zvec"))]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn readiness_reuses_its_disk_checks_briefly() {
        let _g = env_lock();
        use std::os::unix::fs::PermissionsExt;
        let (dir, iam, _full, _bridge) = scope_fixture("cachex");
        let mut st = ready_state(iam);
        st.io_check_ttl = Duration::from_secs(3600);
        let app = router_with_state(st.clone());
        assert_eq!(readyz(&app).await.0, StatusCode::OK);
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let cached = readyz(&app).await.0;
            let mut fresh = st.clone();
            fresh.io_check_ttl = Duration::ZERO;
            let (code, failing, _) = readyz(&router_with_state(fresh)).await;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(cached, StatusCode::OK, "reused within the TTL: no new probe");
            assert_eq!((code, failing), (StatusCode::SERVICE_UNAVAILABLE, vec!["data_dir".to_string()]));
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("readyz-probe")).collect();
        assert!(left.is_empty(), "{left:?}");
        std::env::remove_var("DM_DATA_DIR");
    }

    // The probe never writes through a path it did not create: a symlink (or any file) under
    // the name it picks is left alone and the next name is used.
    #[cfg(unix)]
    #[test]
    fn the_readiness_probe_never_follows_a_planted_symlink() {
        let dir = std::env::temp_dir().join(format!("dmprobe-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("precious");
        std::fs::write(&target, "keep me").unwrap();
        let next = PROBE_SEQ.load(Ordering::SeqCst);
        for n in next..next + 2 {
            std::os::unix::fs::symlink(&target, dir.join(format!(".readyz-probe-{}-{n}", std::process::id()))).unwrap();
        }
        probe_dir(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
        let links = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".readyz-probe")).count();
        assert_eq!(links, 2, "the planted links are not ours to remove, and our own probe is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A request that passed the `closing` check and then waited for the tenant lock while the
    // stop began gets 503 once it has the lock: it never writes after the shutdown flush.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_that_waited_for_the_lock_through_the_close_answers_503() {
        let _g = env_lock();
        let (dir, iam, full, _bridge) = scope_fixture("waitx");
        let st = ready_state(iam);
        let app = router_with_state(st.clone());
        let (code, _) = post_as(&app, "/remember", &full, json!({"text": "zwt before the stop"}), &[]).await;
        assert_eq!(code, StatusCode::OK);
        let handle = st.memory_for("waitx").unwrap();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _g = handle.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = hold_rx.recv_timeout(Duration::from_secs(30));
        });
        held_rx.recv().unwrap();
        let (app2, full2) = (app.clone(), full.clone());
        let req = tokio::spawn(async move { post_as(&app2, "/remember", &full2, json!({"text": "zwt after the flush"}), &[]).await });
        tokio::time::sleep(Duration::from_millis(400)).await; // it is waiting for the lock now
        assert!(!req.is_finished());
        st.closing.store(true, Ordering::SeqCst);
        let _ = hold_tx.send(());
        holder.join().unwrap();
        let (code, v) = req.await.unwrap();
        assert_eq!((code, v["error"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("server is shutting down")));
        assert!(live_rows_with_body(&dir, "waitx", "zwt after the flush").is_empty(), "nothing written");
        std::env::remove_var("DM_DATA_DIR");
    }

    #[test]
    fn permits_bound_how_many_run_at_once() {
        let permits = Arc::new(Permits::new(2));
        let (active, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (permits, active, peak) = (permits.clone(), active.clone(), peak.clone());
                std::thread::spawn(move || {
                    let (_p, _) = permits.acquire();
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(*permits.free.lock().unwrap(), 2, "every permit came back");
        assert_eq!(Permits::new(0).total, 1, "at least one");
    }

    // Embedding outside the tenant lock is bounded: with one save permit, two long saves embed
    // one after the other (not at once, as they would without a bound), and a recall, which
    // has permits of its own, is answered while they run.
    #[cfg(feature = "zvec")]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn embedding_is_bounded_and_a_recall_does_not_queue_behind_saves() {
        let _g = env_lock();
        let (_dir, iam, full, _bridge) = scope_fixture("gatex");
        let slow = Arc::new(SlowEmbedder::new(Duration::from_millis(1500)));
        let mut st = app_state(Arc::new(BearerAuth::from_env().unwrap()), Some(iam));
        st.embedder = slow.clone();
        st.embed_gate = Arc::new(EmbedGate { saves: Permits::new(1), queries: Permits::new(1) });
        let app = router_with_state(st.clone());
        let (code, _) = post_as(&app, "/remember", &full, json!({"text": "zgt the storage pool is healthy"}), &[]).await;
        assert_eq!(code, StatusCode::OK);
        let waits = crate::metrics::embed_permit_waits(crate::metrics::EmbedPool::Save);
        let saves: Vec<_> = (0..2)
            .map(|i| {
                let (app, full) = (app.clone(), full.clone());
                tokio::spawn(async move { post_as(&app, "/remember", &full, json!({"text": format!("zgt long save {i} {SLOW_MARK}")}), &[]).await })
            })
            .collect();
        let t0 = Instant::now();
        while !slow.started.load(Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(30), "the slow embed never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let t = Instant::now();
        let (code, v) = post_as(&app, "/recall", &full, json!({"query": "storage pool", "limit": 5}), &[]).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(t.elapsed() < Duration::from_millis(1200), "the recall waited {:?} behind the saves", t.elapsed());
        for s in saves {
            let (code, v) = s.await.unwrap();
            assert_eq!(code, StatusCode::OK, "{v}");
        }
        assert_eq!(slow.peak.load(Ordering::SeqCst), 1, "the two long saves never embedded at once");
        assert!(crate::metrics::embed_permit_waits(crate::metrics::EmbedPool::Save) >= waits + 2, "each save's permit wait is recorded");
        std::env::remove_var("DM_DATA_DIR");
    }
}
