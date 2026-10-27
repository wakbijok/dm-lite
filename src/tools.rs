//! High-level memory API: the typed guided save tools (per-kind required-field
//! validation) + recall. This is daimon's distinctive layer over the engine.

use crate::config;
use crate::entry::{make_uri, now_ms, Edge, Entry, Kind};
use crate::sqlite::{signal_key, SqliteStore};
use crate::store::MemoryStore;
use anyhow::{anyhow, Result};

/// A graph rider in an expanded recall: the hydrated record plus why it rode along -
/// `hop` (1 = adjacent to a seed), `via` (the uri it was reached from on its best-scoring
/// path; empty when the server predates provenance), `rel` (that edge's relation), and
/// `score` (`seed_weight * decay^hop`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecallRider {
    pub entry: Entry,
    pub hop: u32,
    #[serde(default)]
    pub via: String,
    #[serde(default)]
    pub rel: String,
    #[serde(default)]
    pub score: f64,
}

/// An expanded recall with the graph made visible: content-matched `seeds`, scored `riders`
/// with provenance, and `links` - the edges internal to the whole result set (the
/// mini-subgraph), so a consumer can traverse relations without another round-trip.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecallGraph {
    pub seeds: Vec<Entry>,
    pub riders: Vec<RecallRider>,
    #[serde(default)]
    pub links: Vec<Edge>,
}

/// The local (embedded) memory engine: SQLite store + optional zvec vector index.
pub struct LocalMemory {
    store: SqliteStore,
    /// Scope stamped on every write (scope primitive). `Some("")` = tenant-global writer (the
    /// default; every unscoped deployment). `Some(s)` = scoped writer. `None` = this identity
    /// cannot write at all (a scope-bound token minted without a write scope - decision Q3).
    /// Local mode: from `DM_SCOPE`; server mode: set per request from the token (never from
    /// the client payload - anti confused-deputy).
    write_scope: Option<String>,
    /// Agent identity for THIS request's view (security audit 11-08, High #1). Some(a): reads
    /// hide other agents' `agents/<b>/...` trees, writes into another agent's tree are
    /// rejected, and mutations require a visible target. None = agent-less (full legacy).
    agent_view: Option<String>,
    /// Why this request has no write scope, when the reason is not simply a read-only token
    /// (server mode, per request): the tenant requires an explicit write scope the request did
    /// not name (`EXPLICIT_WRITE_SCOPE_REQUIRED`), or an adapter asserted a write scope its
    /// token's cap does not cover. `write_scope` is then None, and every write is refused with
    /// this message rather than the read-only-token reason.
    write_refusal: Option<String>,
    /// Is this request's identity scope-unbound (server mode, per request): a token minted
    /// without scope grants that is not an adapter, whatever scope it names in
    /// `X-DM-Write-Scope`? That is the operator, who keeps the migration levers of
    /// `import_record_at` when it writes as a room. False unless set (fail closed).
    scope_unbound: bool,
    /// Shared (`Arc`) so work outside the tenant lock can reach it: `dmem serve` re-embeds the
    /// records a keyword-only period left without a vector while requests go on.
    #[cfg(feature = "zvec")]
    vindex: Option<std::sync::Arc<crate::zvec_index::ZvecIndex>>,
    #[cfg(feature = "zvec")]
    embedder: std::sync::Arc<dyn crate::embedder::Embedder>,
    /// Embeddings this request's caller computed BEFORE taking the tenant lock (server mode, set
    /// per request under the lock and cleared after): a save or recall whose text is here uses
    /// them instead of embedding while it holds the lock. Empty otherwise.
    #[cfg(feature = "zvec")]
    pre: PreEmbedded,
    /// Embeddings this handle had to compute itself (texts not in `pre`). In server mode each
    /// one ran while the tenant lock was held.
    #[cfg(feature = "zvec")]
    engine_embeds: std::sync::atomic::AtomicU64,
}

/// The texts a request will embed, known before it takes the tenant lock: record bodies it
/// will save (`Embedder::embed_chunks`) and recall queries (`Embedder::embed`). The server
/// embeds them first and hands the vectors over (`PreEmbedded`), so the tenant lock is held
/// only for the SQLite write and the vector upsert, never for the model. A text missing here
/// is still embedded, under the lock: the plan is an optimisation, never a correctness input.
#[derive(Debug, Default, Clone)]
pub struct EmbedPlan {
    pub docs: Vec<String>,
    pub queries: Vec<String>,
}

impl EmbedPlan {
    pub fn doc(body: impl Into<String>) -> Self {
        Self { docs: vec![body.into()], queries: Vec::new() }
    }
    pub fn query(q: impl Into<String>) -> Self {
        Self { docs: Vec::new(), queries: vec![q.into()] }
    }
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty() && self.queries.is_empty()
    }
}

/// Embeddings computed from an `EmbedPlan`, keyed by their exact text, so a hit is exactly
/// the vector the engine would have computed itself (same embedder, same input).
#[cfg(feature = "zvec")]
#[derive(Default)]
pub struct PreEmbedded {
    chunks: std::collections::HashMap<String, Vec<Vec<f32>>>,
    queries: std::collections::HashMap<String, Vec<f32>>,
}

#[cfg(feature = "zvec")]
impl PreEmbedded {
    /// Embed the plan's texts with `embedder` (the one the tenant handle uses). Call it
    /// without holding the tenant lock: this is the slow part (a long body is up to 64 model
    /// windows).
    pub fn compute(embedder: &dyn crate::embedder::Embedder, plan: &EmbedPlan) -> Self {
        use crate::metrics::{embed, EmbedPhase};
        let mut out = Self::default();
        for d in &plan.docs {
            if !out.chunks.contains_key(d) {
                let t = std::time::Instant::now();
                out.chunks.insert(d.clone(), embedder.embed_chunks(d));
                embed(EmbedPhase::BeforeLock, t.elapsed());
            }
        }
        for q in &plan.queries {
            if !out.queries.contains_key(q) {
                let t = std::time::Instant::now();
                out.queries.insert(q.clone(), embedder.embed(q));
                embed(EmbedPhase::BeforeLock, t.elapsed());
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.queries.is_empty()
    }

    /// Add another computation's vectors (the server embeds a request's queries and its record
    /// bodies under separate permits).
    pub fn merge(&mut self, other: PreEmbedded) {
        self.chunks.extend(other.chunks);
        self.queries.extend(other.queries);
    }
}

/// The embedder, loaded ONCE per process and shared (Arc). Loading the model is expensive and
/// the server opens a tenant store per request, so caching it keeps recall fast and the daemon's
/// RSS stable (~200MB warm) instead of re-mmapping per request. The daemon is a managed service
/// (launchd / systemd): its RAM is reclaimed by STOPPING the service, not by in-process eviction
/// (macOS does not return freed model memory to the OS anyway - verified).
#[cfg(feature = "zvec")]
fn make_embedder() -> std::sync::Arc<dyn crate::embedder::Embedder> {
    loaded_embedder().0.clone()
}

/// The process-wide embedder and, when the model did not load and the placeholder stands in,
/// why (the refusal or the degraded warning carries it).
#[cfg(feature = "zvec")]
static EMBEDDER: std::sync::OnceLock<(std::sync::Arc<dyn crate::embedder::Embedder>, Option<String>)> =
    std::sync::OnceLock::new();

#[cfg(feature = "zvec")]
fn loaded_embedder() -> &'static (std::sync::Arc<dyn crate::embedder::Embedder>, Option<String>) {
    EMBEDDER.get_or_init(build_embedder)
}

/// The process-wide embedder (loaded once, shared): what every tenant handle embeds with.
#[cfg(feature = "zvec")]
pub fn shared_embedder() -> std::sync::Arc<dyn crate::embedder::Embedder> {
    make_embedder()
}

/// Warm the process-wide embedder cache up front (server startup), so the FIRST recall does not
/// pay the model load on a request thread. Subsequent calls reuse the cached instance.
#[cfg(feature = "zvec")]
pub fn warm_embedder() {
    // One line on startup so a cache miss is visible (names the embedder, model, and HF cache dir,
    // and whether the model is already cached or will download on first use). See `dmem doctor`.
    let d = crate::embedder::active_embedder_diag();
    if d.neural {
        eprintln!(
            "dmem: embedder={} model={} cache={} ({})",
            d.name,
            d.model_id.as_deref().unwrap_or("?"),
            d.cache_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "unknown".into()),
            if d.cache_present {
                "cached"
            } else if crate::embedder::hf_hub_offline() {
                "not cached, and HF_HUB_OFFLINE is set: no download"
            } else {
                "will download on first use (needs network)"
            },
        );
    }
    let _ = make_embedder();
}

/// Construct the best available embedder (called once, behind the `make_embedder` cache). When
/// the model does not load, the placeholder stands in and the error comes back with it:
/// `enforce_model_requirement` refuses with it, or warns with it under `DM_REQUIRE_MODEL=0`.
#[cfg(feature = "zvec")]
fn build_embedder() -> (std::sync::Arc<dyn crate::embedder::Embedder>, Option<String>) {
    use crate::embedder::Embedder;
    use std::sync::Arc;
    type Loaded = std::result::Result<Arc<dyn Embedder>, String>;
    #[cfg(feature = "fastembed")]
    let loaded: Loaded = crate::embedder::FastEmbedder::new()
        .map(|e| Arc::new(e) as Arc<dyn Embedder>)
        .map_err(|e| format!("fastembed: {e:#}"));
    #[cfg(all(feature = "candle", not(feature = "fastembed")))]
    let loaded: Loaded = crate::embedder::CandleEmbedder::new()
        .map(|e| Arc::new(e) as Arc<dyn Embedder>)
        .map_err(|e| format!("candle: {e:#}"));
    #[cfg(all(feature = "model2vec", not(feature = "fastembed"), not(feature = "candle")))]
    let loaded: Loaded = crate::embedder::Model2VecEmbedder::new()
        .map(|e| Arc::new(e) as Arc<dyn Embedder>)
        .map_err(|e| format!("model2vec: {e:#}"));
    // no model in this build: the placeholder, and nothing failed to load
    #[cfg(not(any(feature = "fastembed", feature = "candle", feature = "model2vec")))]
    let loaded: Loaded = Ok(Arc::new(crate::embedder::HashEmbedder::new()));
    match loaded.and_then(|e| probe_embedder(&*e).map(|()| e)) {
        Ok(e) => (e, None),
        Err(e) => (Arc::new(crate::embedder::HashEmbedder::new()), Some(e.replace('\n', " "))),
    }
}

/// A model that loads can still be the wrong one: a checkpoint that is not `DIM`-d, or a
/// tokenizer that does not match its weights (the embedder then answers every text with a zero
/// vector). Embed one probe text and take only a finite, non-zero, `DIM`-long vector; anything
/// else counts as a model that did not load, so it meets the same refusal.
#[cfg(feature = "zvec")]
fn probe_embedder(e: &dyn crate::embedder::Embedder) -> std::result::Result<(), String> {
    use crate::embedder::DIM;
    let v = e.embed("dmem embedding probe: the quick brown fox jumps over the lazy dog");
    let bad = if v.len() != DIM {
        format!("a {}-d vector", v.len())
    } else if v.iter().any(|x| !x.is_finite()) {
        "a vector with non-finite values".to_string()
    } else if v.iter().all(|x| *x == 0.0) {
        "a zero vector".to_string()
    } else {
        return Ok(());
    };
    let model = crate::embedder::active_embedder_diag().model_id.unwrap_or_else(|| e.name().to_string());
    Err(format!("{}: {model} embeds a probe text as {bad}; dmem needs a non-zero {DIM}-d vector", e.name()))
}

/// `DM_REQUIRE_MODEL` (`required`, the default): refuse unless a real embedding model is active.
/// `embedder_name` is the active embedder's `Embedder::name` (None = a build without vectors).
/// "hash" is the placeholder the model loaders fall back to when the model cannot load.
pub fn check_model_requirement(embedder_name: Option<&str>, required: bool) -> Result<()> {
    if !required {
        return Ok(());
    }
    match embedder_name {
        Some(name) if name != "hash" => Ok(()),
        _ => Err(anyhow!("{}", model_refusal(embedder_name))),
    }
}

/// The one error for a required model that is not there: which model, why it did not load, the
/// cache dmem looked in, how to pre-install the model, and the escape hatch.
fn model_refusal(embedder_name: Option<&str>) -> String {
    const HATCH: &str = "The model is required unless DM_REQUIRE_MODEL is set to 0 (or false, no), which runs on \
                         the placeholder with degraded recall: for tests and development only.";
    let d = crate::embedder::active_embedder_diag();
    match (embedder_name, d.model_id) {
        (Some(_), Some(model)) => {
            let cause = model_load_error().map(|e| format!(" ({e})")).unwrap_or_default();
            let cache = d.cache_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "unknown".into());
            let present = if d.cache_present { "the model is there" } else { "the model is not there" };
            format!(
                "the embedding model {model} did not load{cause}. Memory recall needs it, so dmem does not fall back \
                 to the placeholder hash embedder. Model cache: {cache} ({present}; HUGGINGFACE_HUB_CACHE or HF_HOME \
                 moves it). To pre-install the model, run dmem once with network access (it downloads into that \
                 cache), or copy a cache that holds it there (README: Offline / air-gapped); `dmem doctor` shows the \
                 cache and whether the model is in it. {HATCH}"
            )
        }
        _ => format!(
            "this dmem build has no embedding model (it was built without the candle feature the release binaries \
             ship). Memory recall needs one, so dmem does not run on the placeholder hash embedder or keyword-only: \
             use a release binary or build with --features dist. {HATCH}"
        ),
    }
}

/// Why the process-wide model did not load (None: it loaded, or has not been loaded; this never
/// loads it).
fn model_load_error() -> Option<&'static str> {
    #[cfg(feature = "zvec")]
    if let Some((_, Some(e))) = EMBEDDER.get() {
        return Some(e.as_str());
    }
    None
}

/// Apply `DM_REQUIRE_MODEL` to the embedder this process runs its engine on (`dmem serve` at
/// start, a local tenant at open): refuse on the placeholder (`check_model_requirement`), or,
/// opted out, say once on stderr that memory recall is degraded.
pub fn enforce_model_requirement(embedder_name: Option<&str>) -> Result<()> {
    check_model_requirement(embedder_name, config::require_model())?;
    if embedder_name.is_none_or(|n| n == "hash") {
        warn_degraded(embedder_name);
    }
    Ok(())
}

/// The loud one-line warning for a placeholder allowed by `DM_REQUIRE_MODEL=0`, once per process.
fn warn_degraded(embedder_name: Option<&str>) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let why = match (embedder_name, crate::embedder::active_embedder_diag().model_id) {
            (Some(_), Some(model)) => format!(
                "the placeholder hash embedder is active (keyword overlap, not semantics): the embedding model {model} \
                 did not load{}",
                model_load_error().map(|e| format!(" ({e})")).unwrap_or_default()
            ),
            (Some(_), None) => "the placeholder hash embedder is active (keyword overlap, not semantics): this build \
                                has no embedding model"
                .to_string(),
            (None, _) => "this build has no embedding model: recall is keyword-only".to_string(),
        };
        eprintln!("dmem: WARNING memory recall is degraded: {why}; DM_REQUIRE_MODEL=0 allows it, for tests and development only");
    });
}

/// A tenant whose vector index did not open while `DM_REQUIRE_VECTORS` is set: its open fails
/// with this error instead of degrading to keyword-only recall.
#[derive(Debug)]
pub struct VectorIndexRequired(pub String);

impl std::fmt::Display for VectorIndexRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DM_REQUIRE_VECTORS is set and the vector index did not open: {}", self.0)
    }
}

impl std::error::Error for VectorIndexRequired {}

/// A refusal by the engine's authorization and policy guards, with a message that is safe to
/// show the caller as is: it names the rule that refused, never storage paths or internals.
/// The server answers it with 400 and this message (other errors keep their generic body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(String);

impl Refusal {
    pub fn new(msg: impl Into<String>) -> Self {
        Refusal(msg.into())
    }
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// The refusal of a write that did not name its scope on a tenant that requires it (tenant
/// policy `require_explicit_write_scope`). The wording is part of the HTTP contract.
pub const EXPLICIT_WRITE_SCOPE_REQUIRED: &str = "this tenant requires an explicit write scope";

/// The refusal of an import by a write-restricted identity that sets `importance` above the
/// kind's default (a ranking lever in the persona boot layer and recall).
pub const IMPORTANCE_IS_SET_BY_THE_OPERATOR: &str = "importance is set by the operator";

/// The refusal of an import by a write-restricted identity that sets `created_ms`
/// (backdating is a migration capability of full-tenant writers only).
pub const CREATED_MS_IS_SET_BY_THE_OPERATOR: &str = "created_ms is set by the operator";

/// Shorthand: an `Err` carrying a `Refusal`.
fn refuse<T>(msg: impl Into<String>) -> Result<T> {
    Err(Refusal::new(msg).into())
}

/// Is `rel` an edge label a write-restricted identity may create? One lowercase token: a
/// letter, then letters, digits, `_` or `-`, at most 32 characters in all
/// (`^[a-z][a-z0-9_-]{0,31}$`). An edge carries no scope, so its label must not carry
/// content: free text in a rel is readable by everyone who sees both endpoints.
pub(crate) fn valid_rel(rel: &str) -> bool {
    let b = rel.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// The write scope an embedded session takes from `--scope`/`DM_WRITE_SCOPE` (`write`), given
/// its `DM_SCOPE` binding (`bound`): Some(w) = stamp writes with w (reads stay as they are),
/// None = keep the session's own. A `DM_SCOPE` session is already bound to its scope, so
/// naming the same one is a no-op and naming another is refused rather than ignored.
fn embedded_write_scope(bound: Option<String>, write: Option<String>) -> Result<Option<String>> {
    match (bound, write) {
        (_, None) => Ok(None),
        (None, Some(w)) => Ok(Some(w)),
        (Some(s), Some(w)) if s == w => Ok(None),
        (Some(s), Some(_)) => {
            anyhow::bail!("DM_SCOPE={s} binds this session's writes; --scope/DM_WRITE_SCOPE cannot name another scope")
        }
    }
}

fn require(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(anyhow!("missing required field: {}", field))
    } else {
        Ok(())
    }
}

pub(crate) fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    line.chars().take(80).collect::<String>()
}

/// Extract the inner text of every `[[...]]` reference in a body (the wikilink convention the
/// Save Discipline tells agents to use). Returns the raw names; the caller slugs and resolves them.
pub(crate) fn parse_wikilinks(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("[[") {
        rest = &rest[i + 2..];
        match rest.find("]]") {
            Some(j) => {
                // Pipe alias form [[Target|shown text]]: the link target is the part before
                // the pipe (ours ate the
                // whole thing and produced an unresolvable slug).
                let name = rest[..j].split('|').next().unwrap_or("").trim();
                // A well-formed [[name]] carries no brackets inside; skip nested/garbled captures.
                if !name.is_empty() && !name.contains('[') && !name.contains(']') {
                    out.push(name.to_string());
                }
                rest = &rest[j + 2..];
            }
            None => break,
        }
    }
    out
}

/// Title-derived aliases for an entity: the title itself, the title with any parenthetical
/// stripped ("Larkdesk (Windows)" -> "Larkdesk"), and each `/`-separated variant.
/// Deliberately cheap - richer aliases are a curation concern on the entity record itself.
pub(crate) fn entity_aliases(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |out: &mut Vec<String>, s: &str| {
        let s = s.trim();
        if s.len() >= 2 && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    };
    push(&mut out, title);
    let mut base = String::new();
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => base.push(c),
            _ => {}
        }
    }
    let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
    push(&mut out, &base);
    for part in base.split('/') {
        push(&mut out, part);
    }
    out
}

/// True when `text` contains `name` as a whole word, exact case. Word chars are alphanumeric
/// plus `-` and `_`, so `dm-lite` matches in prose but never inside `dm-lite-poc`.
pub(crate) fn mentions_word(text: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '-' || c == '_');
    for (i, _) in text.match_indices(name) {
        let before_ok = text[..i].chars().next_back().is_none_or(boundary);
        let after_ok = text[i + name.len()..].chars().next().is_none_or(boundary);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Stamp write attribution into a record's tags as `author:<agent>`, unless an author tag is
/// already present (the first attribution wins; a re-save through another path must not
/// silently re-assign a record). `None` (agent-less callers: embedded mode, legacy tokens)
/// leaves the tags untouched, so pre-agent behaviour is byte-identical.
pub(crate) fn stamp_author(tags: &mut Vec<String>, author: Option<&str>) {
    if let Some(a) = author {
        if !tags.iter().any(|t| t.starts_with("author:")) {
            tags.push(format!("author:{a}"));
        }
    }
}

/// Render a domain-entity record body from a name, kind, and key/value attributes (the
/// knowledge-graph layer). Attributes go in a small structured block; relations between entities
/// are edges (the graph), not body content. The name becomes the record title.
/// The body `log_decision` saves. Pure, so a caller can embed it before taking the tenant lock
/// (`EmbedPlan`); the typed save builds its body with the same function.
pub fn decision_body(title: &str, context: &str, decision: &str, rationale: &str) -> String {
    format!("# {}\n\n**Context:** {}\n\n**Decision:** {}\n\n**Rationale:** {}\n", title, context, decision, rationale)
}

/// The body `log_lesson` saves (see `decision_body`).
pub fn lesson_body(title: &str, lesson: &str) -> String {
    format!("# {}\n\n**Lesson:** {}\n", title, lesson)
}

/// The body `log_incident` saves (see `decision_body`).
pub fn incident_body(title: &str, impact: &str, resolution: &str) -> String {
    format!("# {}\n\n**Impact:** {}\n\n**Resolution:** {}\n", title, impact, resolution)
}

/// The body `add_reminder` saves (see `decision_body`).
pub fn reminder_body(title: &str, text: &str) -> String {
    format!("# {}\n\n**Reminder:** {}\n", title, text)
}

/// The body `log_runbook` saves (see `decision_body`).
pub fn runbook_body(title: &str, steps: &str) -> String {
    format!("# {}\n\n**Runbook:** {}\n", title, steps)
}

/// The body `log_convention` saves (see `decision_body`).
pub fn convention_body(title: &str, rule: &str) -> String {
    format!("# {}\n\n**Convention:** {}\n", title, rule)
}

pub fn entity_body(kind: Kind, name: &str, attrs: &[(String, String)], desc: &str) -> String {
    let mut s = format!("# {}\n\n**Entity:** {}\n", name, kind.as_str());
    for (k, v) in attrs {
        if !k.trim().is_empty() {
            s.push_str(&format!("**{}:** {}\n", k.trim(), v.trim()));
        }
    }
    let desc = desc.trim();
    if !desc.is_empty() {
        s.push_str(&format!("\n{}\n", desc));
    }
    s
}

/// Modest, deterministic runtime-signal multiplier, clamped to [1.0, 1.25]. It NUDGES
/// ranking: items at adjacent or deeper ranks may be reordered, but a clearly higher-ranked
/// hit (a large base-score gap) is never displaced, because the multiplier is bounded. This
/// is a bounded nudge, NOT an order-preserving guarantee at every rank. Components (all
/// small): record importance, recency of last access, log access frequency; `last_access_ms
/// <= 0` (never accessed) contributes no recency. Deterministic: `now_ms` is passed in.
fn signal_boost(importance: i64, access_count: i64, last_access_ms: i64, now_ms: i64) -> f64 {
    let importance_norm = (importance as f64 / 100.0).clamp(0.0, 1.0);
    let recency = if last_access_ms <= 0 {
        0.0
    } else {
        let age_days = ((now_ms - last_access_ms).max(0) as f64) / 86_400_000.0;
        1.0 / (1.0 + age_days)
    };
    let freq = (1.0 + access_count.max(0) as f64).ln();
    (1.0 + 0.05 * importance_norm + 0.05 * recency + 0.02 * freq).clamp(1.0, 1.25)
}

/// Pure relevance gate: which uris survive the floor. Both scores are "higher = better" (cosine
/// similarity in [-1, 1] for the vector channel; `-bm25` >= 0 for the keyword channel). Clearing a
/// channel means magnitude >= the absolute floor AND >= `rel_ratio` * the channel's top magnitude;
/// the relative clause is SKIPPED when the channel's top score is <= 0, so a negative top can never
/// invert into admitting WORSE hits.
///
/// COSINE is the floor when a vector channel exists (hybrid mode): a hit survives iff its cosine
/// clears the cosine gate. The keyword channel does NOT independently bypass cosine, because a
/// shared common word (e.g. an off-topic query and a filler both containing "rules") would
/// otherwise admit semantically-irrelevant junk - exactly the pollution we are removing. Keyword
/// relevance still shapes the RRF RANKING among survivors upstream; here it only matters for the
/// INFINITY sentinel (empty/short query -> recent() boot rows), which always survives so the
/// SessionStart/persona injection is never floored out.
///
/// When the vector channel is EMPTY (keyword-only build, or vector search failed) cosine is
/// unavailable, so the keyword `-bm25` RELATIVE gate is the floor (bm25 is corpus-relative, so the
/// scale-free ratio trims the weak tail; the absolute keyword floor stays permissive). In that mode
/// an off-topic query that shares a term can still leak a weak keyword hit - a documented limit of
/// the keyword-only fallback; the off-topic-injects-zero guarantee lives in the cosine gate.
fn floor_survivors(
    kw: &[(String, f64)],
    vec: &[(String, f32)],
    f: &config::RecallFloor,
    small_corpus_max: usize,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    // Keyword-only mode: no cosine to gate on, so the bm25 relative gate is the floor.
    if vec.is_empty() {
        let top_kw = kw.iter().map(|(_, s)| *s).fold(f64::NEG_INFINITY, f64::max);
        let survivors: HashSet<String> = kw
            .iter()
            .filter(|(_, s)| *s >= f.abs_keyword && (top_kw <= 0.0 || *s >= f.rel_ratio * top_kw))
            .map(|(u, _)| u.clone())
            .collect();
        // Small-corpus guard (09-08-2026): BM25 absolute
        // magnitudes are meaningless when FTS matched only a handful of docs (IDF -> 0 on a
        // tiny/fresh corpus), so an absolute floor calibrated on a real corpus wrongly gates
        // everything and a fresh install looks broken. When the floor rejects the WHOLE pool
        // and the pool is small enough to inject anyway (<= small_corpus_max), trust MATCH
        // membership - FTS only returns docs containing the query terms - and keep them all
        // in rank order. Partial survival or a bigger pool means the magnitudes are
        // discriminating, so the gate stands.
        if survivors.is_empty() && !kw.is_empty() && kw.len() <= small_corpus_max {
            return kw.iter().map(|(u, _)| u.clone()).collect();
        }
        return survivors;
    }
    // Hybrid: cosine is the floor.
    let top_c = vec.iter().map(|(_, s)| *s as f64).fold(f64::NEG_INFINITY, f64::max);
    let mut keep: HashSet<String> = vec
        .iter()
        .filter(|(_, s)| {
            let s = *s as f64;
            s >= f.abs_cosine && (top_c <= 0.0 || s >= f.rel_ratio * top_c)
        })
        .map(|(u, _)| u.clone())
        .collect();
    // The empty/short-query sentinel (recent() boot rows) is never gated out.
    for (u, s) in kw {
        if *s == f64::INFINITY {
            keep.insert(u.clone());
        }
    }
    keep
}

/// The marker beside a tenant's vector directory recording that its index holds one slot per
/// record - per (uri, scope) - rather than the pre-0.3.7 one vector per uri.
#[cfg(feature = "zvec")]
fn slot_marker(vdir: &std::path::Path) -> std::path::PathBuf {
    let name = vdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    vdir.with_file_name(format!("{name}.scope-slots"))
}

/// The checkpoint beside a tenant's vector directory while its slot migration is under way:
/// the last uri whose slots are done (the work list is in uri order), so an interrupted
/// migration resumes after it instead of starting over.
#[cfg(feature = "zvec")]
/// The marker of an index that was created fresh while its store already had records (a
/// quarantined torn index, or one moved aside by hand): those records have no vector until the
/// backfill writes them, and hybrid recall gates on cosine, so it would not find them. Written
/// before the backfill starts and removed when it ends, so an interrupted one resumes.
#[cfg(feature = "zvec")]
pub(crate) fn backfill_marker(vdir: &std::path::Path) -> std::path::PathBuf {
    let name = vdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    vdir.with_file_name(format!("{name}.backfill"))
}

#[cfg(feature = "zvec")]
fn backfill_progress(vdir: &std::path::Path) -> std::path::PathBuf {
    let name = vdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    vdir.with_file_name(format!("{name}.backfill.progress"))
}

/// Give every record in `store` its vectors in `vindex` (each uri's live rows, per scope),
/// resuming after the last uri a previous, interrupted run finished; then clear the marker.
/// Returns the vectors written. The local engine runs it when it opens a marked index; `dmem
/// serve` runs it on a background thread instead, so a large store does not hold up readiness.
#[cfg(feature = "zvec")]
pub fn backfill_vectors(
    store: &SqliteStore,
    vindex: &crate::zvec_index::ZvecIndex,
    embedder: &dyn crate::embedder::Embedder,
    vdir: &std::path::Path,
) -> Result<usize> {
    let progress = backfill_progress(vdir);
    // Every uri the store has ever held (global ones too, which the slot migration skips),
    // in uri order (`uris_changed_since` sorts by uri, binary collation = Rust str order), so
    // the checkpoint resumes exactly where an interrupted run stopped.
    let done_through = std::fs::read_to_string(&progress).ok().filter(|s| !s.is_empty());
    let uris = store.uris_changed_since(i64::MIN / 2)?;
    let todo: Vec<&String> = uris.iter().filter(|u| done_through.as_deref().is_none_or(|d| u.as_str() > d)).collect();
    if !todo.is_empty() {
        eprintln!(
            "dmem: backfilling the vector index from the database: {} of {} uri(s) to do{}",
            todo.len(),
            uris.len(),
            if done_through.is_some() { " (resuming an interrupted run)" } else { "" }
        );
    }
    let mut n = 0usize;
    for (i, uri) in todo.iter().enumerate() {
        n += migrate_uri_slots(store, vindex, embedder, uri)?;
        write_slot_checkpoint(&progress, uri);
        if (i + 1) % 100 == 0 {
            eprintln!("dmem: vector index backfill: {}/{} uri(s) done", i + 1, todo.len());
        }
    }
    let _ = vindex.flush();
    let _ = std::fs::remove_file(&progress);
    let _ = std::fs::remove_file(backfill_marker(vdir));
    Ok(n)
}

fn slot_progress(vdir: &std::path::Path) -> std::path::PathBuf {
    let name = vdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    vdir.with_file_name(format!("{name}.scope-slots.progress"))
}

/// Tenants whose vector index predates per-scope slots (a `vectors/<tenant>` directory with a
/// tenant database and no slot marker): the ones `dmem serve` migrates at start-up.
#[cfg(feature = "zvec")]
pub fn tenants_pending_slot_migration() -> Result<Vec<String>> {
    let data = config::data_dir()?;
    let rd = match std::fs::read_dir(data.join("vectors")) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for ent in rd {
        let ent = ent?;
        if !ent.file_type()?.is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let has_db = data.join("tenants").join(format!("{name}.db")).exists();
        if has_db && name == config::canonical_tenant(&name) && !slot_marker(&ent.path()).exists() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Open a tenant's vector index; on an existing index written before per-scope slots, run the
/// one-time slot migration first, before any request is served from it (`dmem serve` opens
/// every pending tenant at start-up; the embedded engine at its first open). A fresh index is
/// born in the new layout. The migration is idempotent and resumable: a checkpoint records
/// the last uri done, so an interrupted run (a restart mid-way) picks up after it; when it
/// fails, the marker is not written and the next open resumes. Recall degrades to
/// keyword-only only if the index itself cannot open, and not even then when `require_vectors`
/// (`DM_REQUIRE_VECTORS`) is set: the open then fails with `VectorIndexRequired`.
#[cfg(feature = "zvec")]
fn open_vector_index(
    store: &SqliteStore,
    vdir: &std::path::Path,
    embedder: &dyn crate::embedder::Embedder,
    require_vectors: bool,
    defer_backfill: bool,
) -> Result<Option<crate::zvec_index::ZvecIndex>> {
    // Never hand zvec a torn index: its native open dies with SIGSEGV on an empty manifest,
    // and a process that crashes opening a tenant crashes on every restart. Move it aside
    // (nothing is deleted) and start a fresh one, which the backfill below fills from the
    // database, the authoritative copy.
    if let Some(why) = crate::zvec_index::torn_reason(vdir) {
        crate::metrics::zvec_failure(crate::metrics::ZvecOp::Open);
        match crate::zvec_index::quarantine(vdir) {
            Ok(to) => eprintln!(
                "dmem: vector index {} is torn ({why}), as an unclean stop leaves it; moved it to {} and starting a fresh index, rebuilt from the database",
                vdir.display(),
                to.display()
            ),
            Err(e) => {
                eprintln!("dmem: vector index {} is torn ({why}) and could not be moved aside ({e:#}); it is not opened", vdir.display());
                if require_vectors {
                    return Err(VectorIndexRequired(format!("torn vector index ({why})")).into());
                }
                return Ok(None);
            }
        }
    }
    // The last run did not close this index cleanly (a crash, a kill, a power-off): it may be
    // torn in a way the file sizes do not show, so open it in a child process first, where a
    // native crash costs the child only. Indexes closed cleanly are not probed.
    if vdir.exists() && crate::zvec_index::dirty_marker(vdir).exists() {
        match crate::zvec_index::probe(vdir) {
            crate::zvec_index::Probe::Opened | crate::zvec_index::Probe::Skipped(_) => {}
            // Another process has it open (a server beside this CLI run): in use, not damaged.
            // The open below then fails on the lock and falls back as it always has.
            crate::zvec_index::Probe::InUse => {}
            crate::zvec_index::Probe::Failed(why) => {
                crate::metrics::zvec_failure(crate::metrics::ZvecOp::Open);
                match crate::zvec_index::quarantine(vdir) {
                    Ok(to) => {
                        let _ = std::fs::remove_file(crate::zvec_index::dirty_marker(vdir));
                        eprintln!(
                            "dmem: vector index {} failed its open probe after an unclean stop ({why}); moved it to {} and starting a fresh index, rebuilt from the database",
                            vdir.display(),
                            to.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("dmem: vector index {} failed its open probe ({why}) and could not be moved aside ({e:#}); it is not opened", vdir.display());
                        if require_vectors {
                            return Err(VectorIndexRequired(format!("vector index failed its open probe ({why})")).into());
                        }
                        return Ok(None);
                    }
                }
            }
        }
    }
    let fresh = !vdir.exists();
    let vindex = match crate::zvec_index::ZvecIndex::open(vdir) {
        Ok(v) => v,
        Err(e) if require_vectors => {
            crate::metrics::zvec_failure(crate::metrics::ZvecOp::Open);
            eprintln!("dmem: zvec vector index unavailable ({e:#}); DM_REQUIRE_VECTORS is set, so the tenant does not open");
            return Err(VectorIndexRequired(format!("{e:#}")).into());
        }
        Err(e) => {
            crate::metrics::zvec_failure(crate::metrics::ZvecOp::Open);
            eprintln!("dmem: zvec vector index unavailable ({:#}); falling back to keyword-only recall", e);
            return Ok(None);
        }
    };
    let marker = slot_marker(vdir);
    let progress = slot_progress(vdir);
    if !fresh && !marker.exists() {
        match migrate_vector_slots(store, &vindex, embedder, &progress) {
            Ok(0) => {}
            Ok(n) => eprintln!("dmem: vector index re-keyed per scope ({n} record vector(s) rebuilt)"),
            Err(e) => {
                crate::metrics::zvec_failure(crate::metrics::ZvecOp::Migrate);
                eprintln!("dmem: vector index per-scope migration failed ({e:#}); it resumes on the next open");
                return Ok(Some(vindex));
            }
        }
    }
    if !marker.exists() {
        let note = "vector slots are per (uri, scope) - written by dmem 0.3.7+; delete to re-run the migration\n";
        if let Err(e) = std::fs::write(&marker, note) {
            eprintln!("dmem: could not write {} ({e}); the slot migration re-runs next open", marker.display());
            return Ok(Some(vindex));
        }
    }
    let _ = std::fs::remove_file(&progress);
    // A fresh index next to a store that already has records: mark it for the backfill
    // (0.3.8 left such an index empty for good, so those records dropped out of recall).
    let marker = backfill_marker(vdir);
    if fresh && !store.uris_changed_since(i64::MIN / 2).map(|u| u.is_empty()).unwrap_or(true) {
        if let Err(e) = std::fs::write(&marker, "this index was created while the database had records: their vectors are being rebuilt; delete to skip\n") {
            eprintln!("dmem: could not write {} ({e}); the records already saved get no vector until `dmem reindex_embeddings`", marker.display());
        }
    }
    if marker.exists() && !defer_backfill {
        match backfill_vectors(store, &vindex, embedder, vdir) {
            Ok(n) => eprintln!("dmem: vector index backfilled from the database ({n} record vector(s))"),
            Err(e) => eprintln!("dmem: vector index backfill failed ({e:#}); it resumes on the next open"),
        }
    }
    Ok(Some(vindex))
}

/// Record that every uri up to and including `uri` is migrated (write-then-rename, so a crash
/// never leaves a torn checkpoint). Best-effort: a checkpoint that cannot be written only
/// means a resumed run redoes more work.
#[cfg(feature = "zvec")]
fn write_slot_checkpoint(progress: &std::path::Path, uri: &str) {
    let tmp = progress.with_extension("progress.tmp");
    if std::fs::write(&tmp, uri).and_then(|_| std::fs::rename(&tmp, progress)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The per-scope slot migration. Before 0.3.7 the index held ONE vector per uri, under the bare
/// uri key - whichever scope saved last. That key is the GLOBAL slot now, so for every uri a
/// scope has ever held a row of (live or since closed - the vector may still carry its text):
/// rebuild the bare-uri slot from the newest live global row (or clear it when the uri has
/// none), and give each scope with a live row its own slot from its newest live row. Uris only
/// ever held globally are already right and are not touched. Reader gates never apply here:
/// this mirrors what is stored. The work list is in uri order and each finished uri is
/// checkpointed in `progress`, so a run that was interrupted resumes after the last uri it
/// finished. Returns the vectors rebuilt.
#[cfg(feature = "zvec")]
fn migrate_vector_slots(
    store: &SqliteStore,
    vindex: &crate::zvec_index::ZvecIndex,
    embedder: &dyn crate::embedder::Embedder,
    progress: &std::path::Path,
) -> Result<usize> {
    // The uri text exactly as checkpointed (uris may hold any character, so no trimming).
    let done_through = std::fs::read_to_string(progress).ok().filter(|s| !s.is_empty());
    let uris = store.uris_with_scoped_rows()?;
    let total = uris.len();
    // uris_with_scoped_rows sorts by the BINARY collation (byte order), which is how Rust orders str.
    let todo: Vec<String> = match &done_through {
        Some(d) => uris.into_iter().filter(|u| u.as_str() > d.as_str()).collect(),
        None => uris,
    };
    if total > 0 {
        eprintln!(
            "dmem: re-keying the vector index per scope: {} of {total} uri(s) to do{}",
            todo.len(),
            if done_through.is_some() { " (resuming an interrupted run)" } else { "" }
        );
    }
    let mut rebuilt = 0usize;
    for (i, uri) in todo.iter().enumerate() {
        rebuilt += migrate_uri_slots(store, vindex, embedder, uri)?;
        write_slot_checkpoint(progress, uri);
        if (i + 1) % 100 == 0 {
            eprintln!("dmem: vector index re-keying: {}/{} uri(s) done", i + 1, todo.len());
        }
    }
    Ok(rebuilt)
}

/// Rewrite one uri's vectors from its live records (each scope's newest live row), and drop
/// its global vector when it has no live global record: returns the vectors written. `dmem
/// serve` runs it, outside the tenant lock and on its own connection, for every uri written
/// while the tenant's vector index was unavailable, once the index opens again. Hybrid recall
/// gates on cosine, so a record without a vector is not recalled until then.
#[cfg(feature = "zvec")]
pub fn resync_uri_vectors(
    store: &SqliteStore,
    vindex: &crate::zvec_index::ZvecIndex,
    embedder: &dyn crate::embedder::Embedder,
    uri: &str,
) -> Result<usize> {
    migrate_uri_slots(store, vindex, embedder, uri)
}

/// One uri's share of the slot migration (see `migrate_vector_slots`): the vectors rebuilt.
#[cfg(feature = "zvec")]
fn migrate_uri_slots(
    store: &SqliteStore,
    vindex: &crate::zvec_index::ZvecIndex,
    embedder: &dyn crate::embedder::Embedder,
    uri: &str,
) -> Result<usize> {
    let mut rebuilt = 0usize;
    match store.newest_live(uri, Some(""))? {
        Some(g) => {
            vindex.upsert_chunks(uri, "", &embedder.embed_chunks(&g.body))?;
            rebuilt += 1;
        }
        None => vindex.remove(uri, "")?,
    }
    for scope in store.live_scopes_of(uri)?.into_iter().filter(|s| !s.is_empty()) {
        if let Some(e) = store.newest_live(uri, Some(&scope))? {
            vindex.upsert_chunks(uri, &scope, &embedder.embed_chunks(&e.body))?;
            rebuilt += 1;
        }
    }
    Ok(rebuilt)
}

/// Ranked recall hits (best first) cut to ONE record per uri. A reader can see several records
/// under one uri - the global record and its room's copy of the same title, or several rooms'
/// copies for a full-tenant reader - and each is ranked on its own text. Only one takes a
/// place, at the best rank any of them reached, so copies of one title never fill the limit
/// (one uri, one hit, as before per-scope records). Which one: the copy in the reader's own
/// write scope (`own`) if it made the ranking, else the newest; the E4 rule, deterministic
/// rather than decided by near-equal scores. The others add nothing to its score.
fn one_per_uri(scored: Vec<(Entry, f64)>, own: Option<&str>) -> Vec<Entry> {
    let mut order: Vec<String> = Vec::new();
    let mut pick: std::collections::HashMap<String, Entry> = std::collections::HashMap::new();
    for (e, _) in scored {
        match pick.get_mut(&e.uri) {
            None => {
                order.push(e.uri.clone());
                pick.insert(e.uri.clone(), e);
            }
            Some(cur) => {
                let (e_own, cur_own) = (own == Some(e.scope.as_str()), own == Some(cur.scope.as_str()));
                if (e_own && !cur_own) || (e_own == cur_own && e.system_from_ms > cur.system_from_ms) {
                    *cur = e;
                }
            }
        }
    }
    order.into_iter().filter_map(|u| pick.remove(&u)).collect()
}

/// Fusion key of one RECORD - a (uri, scope) pair: hybrid recall ranks records, and one uri can
/// be held by several scopes the reader sees (global and its own room). Length-prefixed, so it
/// is injective.
#[cfg(feature = "zvec")]
fn record_key(uri: &str, scope: &str) -> String {
    format!("{}:{scope}{uri}", scope.len())
}

impl LocalMemory {
    /// Open the embedded-mode tenant ($DM_TENANT, else "default"). `DM_WRITE_SCOPE` (the
    /// global `--scope` flag) stamps this invocation's writes with that scope and leaves reads
    /// as they are, as it does for a full-tenant token on a server; a `DM_SCOPE` session is
    /// already bound to its scope and refuses a different one.
    pub fn open() -> Result<Self> {
        let override_ws = embedded_write_scope(config::scope(), config::write_scope())?;
        let mut m = Self::open_tenant(&config::tenant())?;
        if let Some(w) = override_ws {
            m.write_scope = Some(w);
        }
        Ok(m)
    }

    /// Open a specific tenant's store explicitly. Server mode uses this per request so it
    /// never mutates the process-global $DM_TENANT (which would race under concurrency).
    pub fn open_tenant(tenant: &str) -> Result<Self> {
        // Not deferred: a local caller (the CLI, the hooks) has no background thread to run
        // a pending backfill, so the open runs it (resumable, see `backfill_vectors`).
        #[cfg(feature = "zvec")]
        return Self::open_tenant_inner(tenant, make_embedder(), false);
        #[cfg(not(feature = "zvec"))]
        Self::open_tenant_inner(tenant)
    }

    /// As `open_tenant`, embedding with `embedder` (server mode passes the embedder it also
    /// pre-embeds with, so a pre-embedded vector is exactly what this handle would compute).
    #[cfg(feature = "zvec")]
    pub fn open_tenant_with_embedder(tenant: &str, embedder: std::sync::Arc<dyn crate::embedder::Embedder>) -> Result<Self> {
        // `dmem serve` is the caller: it runs a pending backfill on a background thread
        // (`backfill_pending`), so a tenant with many records does not hold up readiness.
        Self::open_tenant_inner(tenant, embedder, true)
    }

    fn open_tenant_inner(
        tenant: &str,
        #[cfg(feature = "zvec")] embedder: std::sync::Arc<dyn crate::embedder::Embedder>,
        #[cfg(feature = "zvec")] defer_backfill: bool,
    ) -> Result<Self> {
        // The embedding model first (`DM_REQUIRE_MODEL`): a refused open touches no file.
        #[cfg(feature = "zvec")]
        enforce_model_requirement(Some(embedder.name()))?;
        #[cfg(not(feature = "zvec"))]
        enforce_model_requirement(None)?;
        let path = config::db_path(tenant)?;
        let mut store = SqliteStore::open(&path)?;
        // Scope-bound local session (DM_SCOPE): writes stamp that scope and reads narrow to
        // it + global. Unset = full-tenant global session - byte-identical to pre-scope
        // behavior, and the only mode our own deployments use.
        let write_scope = Some(config::scope().unwrap_or_default());
        if let Some(s) = write_scope.as_deref().filter(|s| !s.is_empty()) {
            store.set_read_scopes(Some(vec![s.to_string()]));
        }
        #[cfg(feature = "zvec")]
        let m = {
            let vdir = config::vector_dir(tenant)?;
            let vindex = open_vector_index(&store, &vdir, &*embedder, config::require_vectors(), defer_backfill)?.map(std::sync::Arc::new);
            Self {
                store,
                write_scope,
                agent_view: None,
                write_refusal: None,
                scope_unbound: false,
                vindex,
                embedder,
                pre: PreEmbedded::default(),
                engine_embeds: Default::default(),
            }
        };
        #[cfg(not(feature = "zvec"))]
        let m = {
            if config::require_vectors() {
                return Err(VectorIndexRequired("this dmem build has no vector index (keyword-only)".into()).into());
            }
            Self { store, write_scope, agent_view: None, write_refusal: None, scope_unbound: false }
        };
        Ok(m)
    }

    /// Rebind this engine handle to a reader/writer scope context (scope primitive). Server
    /// mode calls this per request under the tenant lock, from the TOKEN's grants; tests use
    /// it directly. `write_scope`: Some("") = global writer, Some(s) = scoped writer, None =
    /// writes rejected (decision Q3). `read_scopes` None = full tenant.
    pub fn set_scope_context(&mut self, write_scope: Option<&str>, read_scopes: Option<Vec<String>>) {
        self.write_scope = write_scope.map(str::to_string);
        self.store.set_read_scopes(read_scopes);
    }

    /// Mark this request as one whose tenant requires an explicit write scope that the request
    /// did not name (server mode, per request, beside `set_scope_context(None, ..)`): writes are
    /// then refused with `EXPLICIT_WRITE_SCOPE_REQUIRED`. Reads are unaffected.
    pub fn set_explicit_write_scope_missing(&mut self, missing: bool) {
        self.set_write_refusal(missing.then(|| EXPLICIT_WRITE_SCOPE_REQUIRED.to_string()));
    }

    /// Name why this request has no write scope (server mode, per request, beside
    /// `set_scope_context(None, ..)`): every write is then refused with `reason`. None = the
    /// plain read-only-token reason. Reads are unaffected.
    pub fn set_write_refusal(&mut self, reason: Option<String>) {
        self.write_refusal = reason;
    }

    /// Mark this request's identity as scope-unbound (server mode, per request, beside the
    /// scope context): a token minted without scope grants that is not an adapter. Only the
    /// migration levers of `import_record_at` read it.
    pub fn set_scope_unbound(&mut self, unbound: bool) {
        self.scope_unbound = unbound;
    }

    /// Bind this handle to an agent identity's view (persona-tree protection). Server mode
    /// sets it per request from the TOKEN's agent label; None = agent-less.
    pub fn set_agent_view(&mut self, agent: Option<String>) {
        self.agent_view = agent.clone();
        self.store.set_agent_view(agent);
    }

    /// Hand this request's pre-computed embeddings to the engine (server mode, per request,
    /// under the tenant lock, beside the scope context; cleared with `clear_pre_embedded`).
    #[cfg(feature = "zvec")]
    pub fn set_pre_embedded(&mut self, pre: PreEmbedded) {
        self.pre = pre;
    }

    /// Drop the request's pre-computed embeddings (the handle is cached per tenant).
    #[cfg(feature = "zvec")]
    pub fn clear_pre_embedded(&mut self) {
        self.pre = PreEmbedded::default();
    }

    /// Embeddings this handle computed itself so far (texts no caller pre-embedded). In server
    /// mode each one held the tenant lock while the model ran.
    #[cfg(feature = "zvec")]
    pub fn engine_embed_count(&self) -> u64 {
        self.engine_embeds.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Flush this tenant's vector index to disk (`dmem serve` shutdown). Ok(false) when there
    /// is no index to flush.
    pub fn flush_vectors(&self) -> Result<bool> {
        #[cfg(feature = "zvec")]
        if let Some(v) = &self.vindex {
            v.flush()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// This tenant's vector index, shared, for work that runs outside the tenant lock.
    #[cfg(feature = "zvec")]
    pub fn vector_index(&self) -> Option<std::sync::Arc<crate::zvec_index::ZvecIndex>> {
        self.vindex.clone()
    }

    /// The tenant's index still has records to backfill (`backfill_marker`): set only for a
    /// handle opened with the backfill deferred (`dmem serve`), which runs `backfill_vectors`.
    #[cfg(feature = "zvec")]
    pub fn backfill_pending(tenant: &str) -> bool {
        config::vector_dir(tenant).map(|d| backfill_marker(&d).exists()).unwrap_or(false)
    }

    /// The backfill of `tenant`'s index is complete (`dmem serve`'s background run): drop the
    /// marker, so the next open does not run it again.
    #[cfg(feature = "zvec")]
    pub fn finish_backfill(tenant: &str) {
        if let Ok(d) = config::vector_dir(tenant) {
            let _ = std::fs::remove_file(backfill_marker(&d));
        }
    }

    /// Does this tenant have a working vector index (hybrid recall)? False for a keyword-only
    /// build and for an index that failed to open.
    pub fn has_vector_index(&self) -> bool {
        #[cfg(feature = "zvec")]
        return self.vindex.is_some();
        #[cfg(not(feature = "zvec"))]
        false
    }

    /// A record body's vectors: the pre-computed ones when the caller embedded this exact text
    /// before taking the lock, else computed here.
    #[cfg(feature = "zvec")]
    fn body_chunks(&self, body: &str) -> Vec<Vec<f32>> {
        if let Some(c) = self.pre.chunks.get(body) {
            return c.clone();
        }
        self.engine_embeds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::Instant::now();
        let chunks = self.embedder.embed_chunks(body);
        crate::metrics::embed(crate::metrics::EmbedPhase::UnderLock, t.elapsed());
        chunks
    }

    /// A recall query's vector (see `body_chunks`).
    #[cfg(feature = "zvec")]
    fn query_vector(&self, query: &str) -> Vec<f32> {
        if let Some(v) = self.pre.queries.get(query) {
            return v.clone();
        }
        self.engine_embeds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::Instant::now();
        let v = self.embedder.embed(query);
        crate::metrics::embed(crate::metrics::EmbedPhase::UnderLock, t.elapsed());
        v
    }

    /// Writes into the `agents/` tree may only target the caller's own subtree. Everything
    /// outside `agents/` is the shared pool (by design). Agent-less identities: no restriction.
    fn guard_agent_namespace(&self, namespace: &str) -> Result<()> {
        let Some(owner) = crate::entry::split_agents_tree(namespace) else {
            return Ok(());
        };
        match &self.agent_view {
            Some(a) if owner.eq_ignore_ascii_case(a) => Ok(()),
            Some(_) => refuse(format!("namespace '{namespace}' belongs to another agent identity")),
            // No agent label: only a FULLY unrestricted identity may touch the agents/ tree.
            // A scoped principal is a user, not an agent - without this, a scoped token could
            // plant records in an agent's tree that the (typically full-tenant) agent token
            // then reads as its own persona (caught by the 0.3.2 live release smoke). A
            // read-restricted global writer is no exception (see `full_tenant`).
            None if self.full_tenant() => Ok(()),
            None => {
                self.effective_write_scope()?; // a write-denied identity hears that reason
                refuse("scoped identities cannot write into the agents/ tree")
            }
        }
    }

    /// A full-tenant identity: it writes global AND reads the whole tenant (a scope-unbound
    /// token, or one that named '' in `X-DM-Write-Scope`). Only such an identity is trusted
    /// with the tenant-wide mutations: retracting every scope's rows, wiring or cutting any
    /// edge, an agent-less write into the `agents/` tree, the migration levers of
    /// `import_record_at` (which a scope-unbound token keeps in any scope it names), and batch
    /// graph maintenance. An identity that writes global but reads a restricted set (an
    /// adapter promoting with `X-DM-Write-Scope: ''`, or a scoped token minted with a '' write
    /// scope) is a GLOBAL writer, not a full-tenant one: its mutations are bounded to the
    /// global rows, exactly as a room writer's are to its room.
    /// Before, its '' write scope alone made the engine treat it as full tenant, so it could
    /// retract, cut and plant records in scopes it cannot even read.
    fn full_tenant(&self) -> bool {
        self.write_scope.as_deref() == Some("") && !self.store.reads_restricted()
    }

    /// Is this identity anything less than a full-tenant writer? (Scoped, a read-restricted
    /// global writer, or write-denied.)
    fn write_restricted(&self) -> bool {
        !self.full_tenant()
    }

    /// The write scope for a save, or a refusal for a write-denied identity (decision Q3: a
    /// scope-bound token without a write scope cannot write; promotion to global is explicit),
    /// naming the tenant policy when that is why this request has none.
    fn effective_write_scope(&self) -> Result<&str> {
        match (self.write_scope.as_deref(), &self.write_refusal) {
            (Some(ws), _) => Ok(ws),
            (None, Some(reason)) => refuse(reason.clone()),
            (None, None) => refuse("this identity has no write scope (writes are read-only for this token)"),
        }
    }

    /// The scope this request's saves are stamped with ('' = global; also '' for a write-denied
    /// identity, which never saves). What the server echoes after a save.
    pub fn stamped_scope(&self) -> &str {
        self.write_scope.as_deref().unwrap_or("")
    }

    /// The scope a retracting mutation (forget/invalidate) is bounded to: None = every scope
    /// (a full-tenant writer, unchanged behaviour); Some(ws) = only the rows in the identity's
    /// own write scope. A write-denied identity errors here.
    fn mutation_scope(&self) -> Result<Option<&str>> {
        if self.write_restricted() {
            Ok(Some(self.effective_write_scope()?))
        } else {
            Ok(None)
        }
    }

    /// Guard for retracting mutations (forget/invalidate). A scope-restricted writer needs at
    /// least one live row of the uri IN ITS OWN write scope (the mutation then touches exactly
    /// those rows - see `mutation_scope`); whatever other scopes hold under the same uri, and
    /// whichever of them saved last, neither grants nor blocks it. An agent-labeled identity
    /// may not touch another agent's `agents/<b>/...` records: every selected row must pass
    /// the agent-tree gate, so a foreign persona reads as absent here.
    fn guard_mutation_target(&self, uri: &str) -> Result<()> {
        if !self.write_restricted() && self.agent_view.is_none() {
            return Ok(());
        }
        let (live, tree_ok) = self.store.mutation_rows(uri, self.mutation_scope()?)?;
        if live == 0 {
            if self.write_restricted() {
                return refuse("record not found in this identity's write scope");
            }
            return refuse("record not found (or not visible to this identity)");
        }
        if !tree_ok {
            return refuse("record not found (or not visible to this identity)");
        }
        Ok(())
    }

    /// The slots a retracting mutation touches, captured BEFORE it runs (they are resynced
    /// after): the identity's own write scope, or every scope holding a live row for a
    /// full-tenant writer, each with the body its slot embeds now (its newest live row's).
    #[cfg(feature = "zvec")]
    fn mutation_slots(&self, uri: &str) -> Result<Vec<(String, Option<String>)>> {
        let scopes = match self.mutation_scope()? {
            Some(ws) => vec![ws.to_string()],
            None => self.store.live_scopes_of(uri)?,
        };
        scopes
            .into_iter()
            .map(|scope| {
                let body = self.store.newest_live(uri, Some(&scope))?.map(|e| e.body);
                Ok((scope, body))
            })
            .collect()
    }

    /// Bring the vector slots of `uri` (from `mutation_slots`) back in line with the live rows
    /// after a retraction or an invalidation: a slot whose scope has no live row left is
    /// dropped; one whose newest live row now has another body is re-embedded from it; one that
    /// already embeds that body is left alone (an invalidation's remainder is a copy of the cut
    /// segment, so the usual cut costs no embedding). Other scopes' slots of the same uri are
    /// untouched - each record owns its own. Best-effort: the database is the source of truth and has
    /// already committed, so an index hiccup is logged, never returned.
    #[cfg(feature = "zvec")]
    fn resync_vector_slots(&self, uri: &str, slots: &[(String, Option<String>)]) {
        let Some(vindex) = &self.vindex else { return };
        for (scope, before) in slots {
            match self.store.newest_live(uri, Some(scope)) {
                Ok(None) => {
                    let _ = vindex.remove(uri, scope);
                }
                Ok(Some(e)) if before.as_deref() == Some(e.body.as_str()) => {}
                Ok(Some(e)) => {
                    let chunks = self.body_chunks(&e.body);
                    if let Err(err) = vindex.upsert_chunks(&e.uri, &e.scope, &chunks) {
                        crate::metrics::zvec_failure(crate::metrics::ZvecOp::Upsert);
                        eprintln!("dmem: vector index re-embed failed for {uri} ({err:#}); keyword recall unaffected");
                    }
                }
                Err(err) => eprintln!("dmem: vector index resync skipped for {uri} ({err:#})"),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn save(&self, kind: Kind, namespace: &str, title: &str, body: String, importance: i64, tags: Vec<String>, author: Option<&str>) -> Result<String> {
        self.save_valid(kind, namespace, title, body, importance, tags, None, None, author)
    }

    /// As `save`, but with a caller-supplied valid interval (the bitemporal application-time axis).
    /// `valid_from = None` means now; `valid_to = None` means open (still true). The store's put
    /// does the valid-time splitting against any existing segments of this entity. `author`
    /// stamps write attribution (see `stamp_author`); None writes exactly as before.
    #[allow(clippy::too_many_arguments)]
    fn save_valid(&self, kind: Kind, namespace: &str, title: &str, body: String, importance: i64, mut tags: Vec<String>, valid_from: Option<i64>, valid_to: Option<i64>, author: Option<&str>) -> Result<String> {
        // Persona-tree protection (audit High #1): an agent identity may not write into
        // another agent's agents/<b>/... namespace - attribution is a tag, this is the authz.
        self.guard_agent_namespace(namespace)?;
        stamp_author(&mut tags, author);
        let uri = make_uri(namespace, kind, title);
        let mut e = Entry::new_now(
            uri.clone(),
            kind,
            namespace.to_string(),
            title.to_string(),
            body,
            tags,
            importance,
            uri.clone(),
        );
        if let Some(vf) = valid_from {
            e.valid_from_ms = vf;
        }
        e.valid_to_ms = valid_to;
        // Stamp the audience: the engine-side write half of the scope primitive. Callers do
        // not choose this per record - it is identity context (DM_SCOPE locally, the token's
        // write scope on the server), which is what makes it unforgeable from a prompt.
        e.scope = self.effective_write_scope()?.to_string();
        self.save_entry(&e)?;
        Ok(uri)
    }

    /// Put an entry and (under zvec) embed its body into the record's own (uri, scope) slot.
    /// Fail-open: a vector-index hiccup never blocks the canonical SQLite save. Bitemporal
    /// invariant: the hashed-PK upsert overwrites the slot's prior vector, so the index holds
    /// exactly the current valid version of each record - and a scoped save never touches the
    /// global record's slot under the same uri.
    fn save_entry(&self, e: &Entry) -> Result<()> {
        self.store.put(e)?;
        #[cfg(feature = "zvec")]
        if let Some(vindex) = &self.vindex {
            let chunks = self.body_chunks(&e.body);
            if let Err(err) = vindex.upsert_chunks(&e.uri, &e.scope, &chunks) {
                crate::metrics::zvec_failure(crate::metrics::ZvecOp::Upsert);
                eprintln!("dmem: vector index upsert failed for {} ({err:#}); keyword recall unaffected", e.uri);
            }
        }
        Ok(())
    }

    /// Re-embed every live record's body into the vector index, overwriting its stored vector.
    /// Heals records whose embedding predates an embedder fix, e.g. bodies that overflowed bge's
    /// 512 position limit and were stored as a zero vector (invisible to hybrid recall because a
    /// zero vector never clears the cosine floor). The hashed-PK upsert overwrites in place, so
    /// this is idempotent and safe to re-run. Skills are excluded: they never enter the recall
    /// pool. Returns (records re-embedded, of which over 2048 bytes of body).
    #[cfg(feature = "zvec")]
    pub fn reindex_embeddings(&self) -> Result<(usize, usize)> {
        let vindex = self
            .vindex
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vector index unavailable; cannot reindex embeddings"))?;
        let all = self.store.recent(1_000_000)?;
        let mut long = 0usize;
        for e in &all {
            if e.body.len() > 2048 {
                long += 1;
            }
            let chunks = self.embedder.embed_chunks(&e.body);
            vindex.upsert_chunks(&e.uri, &e.scope, &chunks)?;
        }
        Ok((all.len(), long))
    }

    /// Import a record preserving its ORIGINAL creation/valid time (for v1->v2 migration).
    /// System time stays "now" (when we recorded it); valid/created time is the original.
    ///
    /// The same guards as every other write: the `agents/` tree guard, and the scope stamped
    /// from the identity (a write-denied identity is refused). The two migration levers stay
    /// the operator's: a write-restricted identity may neither backdate (`created_ms`) nor
    /// raise `importance` above the kind's default, which ranks the persona boot layer and
    /// recall. It may lower it. Full-tenant writers are unchanged, and so is a scope-unbound
    /// token that names a room (`set_scope_unbound`): it could write the same record global,
    /// which reaches further, and the record still lands in the room it named.
    pub fn import_record_at(&self, kind: Kind, namespace: &str, title: &str, body: &str, created_ms: i64, importance: Option<i64>) -> Result<String> {
        require(title, "title")?;
        self.guard_agent_namespace(namespace)?;
        let scope = self.effective_write_scope()?.to_string();
        if self.write_restricted() && !self.scope_unbound {
            if importance.is_some_and(|i| i > crate::entry::default_importance(kind)) {
                return refuse(IMPORTANCE_IS_SET_BY_THE_OPERATOR);
            }
            if created_ms > 0 {
                return refuse(CREATED_MS_IS_SET_BY_THE_OPERATOR);
            }
        }
        let uri = make_uri(namespace, kind, title);
        let mut e = Entry::new_now(
            uri.clone(),
            kind,
            namespace.to_string(),
            title.to_string(),
            body.to_string(),
            vec![],
            importance.unwrap_or_else(|| crate::entry::default_importance(kind)),
            uri.clone(),
        );
        if created_ms > 0 {
            e.created_ms = created_ms;
            e.valid_from_ms = created_ms;
        }
        e.scope = scope;
        self.save_entry(&e)?;
        Ok(uri)
    }

    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(decision, "decision")?;
        let body = decision_body(title, context, decision, rationale);
        self.save(Kind::Decision, namespace, title, body, 70, vec!["decision".into()], author)
    }

    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(lesson, "lesson")?;
        let body = lesson_body(title, lesson);
        self.save(Kind::AgentLesson, namespace, title, body, 60, vec!["agent_lesson".into()], author)
    }

    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(impact, "impact")?;
        let body = incident_body(title, impact, resolution);
        self.save(Kind::IncidentSummary, namespace, title, body, 65, vec!["incident_summary".into()], author)
    }

    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>, author: Option<&str>) -> Result<String> {
        require(text, "text")?;
        let title = first_line(text);
        self.save_valid(Kind::Memory, namespace, &title, text.to_string(), 50, vec![], valid_from, valid_to, author)
    }

    /// Application-time invalidation: this entity's fact is no longer true from `valid_to_ms` on.
    /// Scope-exact like `forget`: a scoped writer cuts only its own scope's segments.
    pub fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        self.guard_mutation_target(uri)?;
        #[cfg(feature = "zvec")]
        let slots = self.mutation_slots(uri)?;
        let n = self.store.invalidate_in(uri, valid_to_ms, self.mutation_scope()?)?;
        // A cut before a segment's start drops it outright: a slot left without a live row goes
        // (as after forget), the others follow what is left.
        #[cfg(feature = "zvec")]
        if n > 0 {
            self.resync_vector_slots(uri, &slots);
        }
        Ok(n)
    }

    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(text, "text")?;
        let body = reminder_body(title, text);
        self.save(Kind::Reminder, namespace, title, body, 55, vec!["reminder".into()], author)
    }

    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(steps, "steps")?;
        let body = runbook_body(title, steps);
        self.save(Kind::Runbook, namespace, title, body, 60, vec!["runbook".into()], author)
    }

    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(rule, "rule")?;
        let body = convention_body(title, rule);
        self.save(Kind::ProjectConvention, namespace, title, body, 65, vec!["project_convention".into()], author)
    }

    /// Import a record of any kind from a template/file (the write path for persona/protocol).
    /// No attribution here: imports are migration/seeding, preserving the original record as-is.
    pub fn import_record(&self, kind: Kind, namespace: &str, title: &str, body: &str) -> Result<String> {
        require(title, "title")?;
        self.save(kind, namespace, title, body.to_string(), crate::entry::default_importance(kind), vec![], None)
    }

    /// Count of live records per kind (for `dm status`).
    pub fn counts(&self) -> Result<Vec<(String, usize)>> {
        let mut all = self.store.recent(1_000_000)?;
        // recent() excludes skills (they are not recall memory); add them back so status shows the
        // full inventory, including the skill count.
        all.extend(self.store.by_kind("skill", 1_000_000)?);
        let mut map: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for e in &all {
            *map.entry(e.kind.as_str().to_string()).or_default() += 1;
        }
        Ok(map.into_iter().collect())
    }

    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        #[cfg(feature = "zvec")]
        if let Some(vindex) = &self.vindex {
            return self.recall_hybrid(query, limit, vindex);
        }
        // Keyword-only: pull a deeper pool (so rescoring can promote beyond the top-`limit`
        // keyword hits), apply the relevance floor's keyword gate (drops the weak bm25 tail so a
        // 2-match query injects ~2, not the whole pool), then the modest runtime-signal rescoring.
        let pool = (limit * 2).max(10);
        let floor = config::recall_floor();
        let hits: Vec<Entry> = if floor.enabled {
            // No vector channel here, so the bm25 relative gate alone decides membership.
            // recall_scored preserves FTS-rank order and `filter` retains it, so rescore_keyword's
            // positional base stays aligned with bm25 rank.
            let scored = self.store.recall_scored(query, pool)?;
            let kw: Vec<(String, f64)> = scored.iter().map(|(e, s)| (e.uri.clone(), *s)).collect();
            let keep = floor_survivors(&kw, &[], &floor, limit);
            let kept: Vec<Entry> = scored.into_iter().filter(|(e, _)| keep.contains(&e.uri)).map(|(e, _)| e).collect();
            // Operator visibility: a default-on floor that empties a non-empty pool must not look
            // like "matched nothing". Say so, with the top magnitude that was rejected.
            if kept.is_empty() && !kw.is_empty() {
                let top = kw.iter().map(|(_, s)| *s).fold(f64::NEG_INFINITY, f64::max);
                eprintln!(
                    "dmem: recall floor gated all {} keyword hit(s) (top -bm25={:.3}); query {:?} returned nothing. Set DM_RECALL_FLOOR=0 to disable.",
                    kw.len(), top, query.chars().take(50).collect::<String>()
                );
            }
            kept
        } else {
            self.store.recall(query, pool)?
        };
        let out = self.rescore_keyword(hits, limit);
        self.bump_recalled(&out);
        Ok(out)
    }

    /// Recall restricted to namespaces under any of `prefixes` (the knowledge plane's
    /// file-scope rule: a chunk's namespace is its file path, a reader's scope is the set of
    /// folder prefixes they may see). Over-fetches, then filters; empty `prefixes` = no filter.
    pub fn recall_in(&self, query: &str, limit: usize, prefixes: &[String]) -> Result<Vec<Entry>> {
        if prefixes.is_empty() {
            return self.recall(query, limit);
        }
        let pool = self.recall(query, (limit * 6).max(30))?;
        Ok(pool.into_iter()
            .filter(|e| prefixes.iter().any(|p| e.namespace.starts_with(p.as_str())))
            .take(limit)
            .collect())
    }

    /// Hybrid recall: SQLite FTS (keyword) + zvec (dense vector), fused by RRF, then nudged
    /// by runtime signals.
    #[cfg(feature = "zvec")]
    fn recall_hybrid(&self, query: &str, limit: usize, vindex: &crate::zvec_index::ZvecIndex) -> Result<Vec<Entry>> {
        use std::collections::HashMap;
        let pool = (limit * 2).max(10);
        // Pull both channels WITH their magnitudes: keyword hits carry -bm25 (and arrive already
        // hydrated), vector hits carry cosine similarity. Both are keyed per RECORD (uri, scope):
        // one uri can be held by several scopes this reader sees (global and its own room).
        let kw_scored = self.store.recall_scored(query, pool)?;
        let kw: Vec<(String, f64)> = kw_scored.iter().map(|(e, s)| (record_key(&e.uri, &e.scope), *s)).collect();
        let mut entries: HashMap<String, Entry> =
            kw_scored.into_iter().map(|(e, _)| (record_key(&e.uri, &e.scope), e)).collect();
        let qv = self.query_vector(query);
        let vec = self.vector_channel(vindex, &qv, pool, &mut entries)?;

        // Relevance floor: gate channel MEMBERSHIP before fusion. RRF, the signal nudge, and
        // take(limit) below are byte-identical to before; the floor only removes weak pool hits
        // (so a 2-relevant query injects ~2 and an off-topic query injects 0). When disabled, every
        // pool hit passes, reproducing the pre-floor result.
        let mut floor = config::recall_floor();
        // Cosine is embedder-relative: the placeholder HashEmbedder's cosine ~ keyword overlap, not
        // bge-scale semantics, so disable its ABSOLUTE cosine gate (the bm25 + relative gates still
        // apply); a bge-calibrated floor would mis-gate the placeholder.
        if self.embedder.name() == "hash" {
            floor.abs_cosine = f64::NEG_INFINITY;
        }
        let keep = if floor.enabled { Some(floor_survivors(&kw, &vec, &floor, limit)) } else { None };
        let passes = |uri: &str| keep.as_ref().is_none_or(|k| k.contains(uri));

        let k = 60.0_f64;
        let mut score: HashMap<String, f64> = HashMap::new();
        // RRF over the FULL pool order (rank positions unchanged), accumulating only survivors -
        // so disabling the floor yields exactly the prior score map.
        for (rank, (uri, _)) in kw.iter().enumerate() {
            if passes(uri) {
                *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
            }
        }
        for (rank, (uri, _)) in vec.iter().enumerate() {
            if passes(uri) {
                *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
            }
        }
        // Every survivor is already hydrated (keyword hits by recall_scored, vector hits as their
        // own record above). Apply the modest runtime-signal multiplier AFTER RRF: a bounded
        // (<=1.25x) nudge that reorders near-equal scores without overturning a clear gap.
        let now = now_ms();
        let keys: Vec<String> = score.keys().filter_map(|k| entries.get(k).map(|e| signal_key(&e.uri, &e.scope))).collect();
        let sigs = self.store.read_signals(&keys).unwrap_or_default();
        let mut scored: Vec<(Entry, f64)> = Vec::new();
        for (key, rrf) in score {
            let Some(e) = entries.remove(&key) else { continue };
            // Skills surface via the ~/.claude/skills projection, not recall. The keyword channel
            // already excludes them in SQL; the vector channel can still return a skill uri, so drop
            // it here before it pollutes per-prompt context with a full SKILL.md body.
            if e.kind == crate::entry::Kind::Skill {
                continue;
            }
            let (ac, la) = sigs.get(&signal_key(&e.uri, &e.scope)).copied().unwrap_or((0, 0));
            let s = rrf * signal_boost(e.importance, ac, la, now);
            scored.push((e, s));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let out: Vec<Entry> = one_per_uri(scored, self.write_scope.as_deref()).into_iter().take(limit).collect();
        // Operator visibility: the floor emptied a non-empty pool (over-gating), distinct from a
        // genuine no-match. Report the top cosine that was rejected so the threshold can be judged.
        if floor.enabled && out.is_empty() && !(kw.is_empty() && vec.is_empty()) {
            let top_c = vec.iter().map(|(_, s)| *s).fold(f32::NEG_INFINITY, f32::max);
            eprintln!(
                "dmem: recall floor gated all {} pool hit(s) (top cosine={:.3} < abs {:.2}); query {:?} returned nothing. Set DM_RECALL_FLOOR=0 to disable.",
                kw.len() + vec.len(), top_c, floor.abs_cosine, query.chars().take(50).collect::<String>()
            );
        }
        self.bump_recalled(&out);
        Ok(out)
    }

    /// The vector channel of a hybrid recall: up to `pool` records THIS reader may see, as
    /// (record key, cosine), best first, hydrated into `entries`. The index ranks every slot of
    /// the tenant, and slots the reader cannot see only drop out after the search, so for a
    /// reader behind a gate (scopes or an agent view) the nearest `pool` slots may be mostly
    /// other audiences' records - per-scope slots make that likelier, as every room copy of a
    /// title is its own near-duplicate - and in hybrid mode a record missing from this channel
    /// cannot survive the cosine floor. So a gated reader searches again with a larger `k`
    /// until `pool` visible records resolve, the index runs out, or `MAX_VECTOR_SLOTS` is
    /// reached (the vector analogue of the exact SQL limits). An ungated reader sees every live
    /// slot: one search, as before.
    #[cfg(feature = "zvec")]
    fn vector_channel(
        &self,
        vindex: &crate::zvec_index::ZvecIndex,
        qv: &[f32],
        pool: usize,
        entries: &mut std::collections::HashMap<String, Entry>,
    ) -> Result<Vec<(String, f32)>> {
        const MAX_VECTOR_SLOTS: usize = 1024;
        let mut k = pool;
        loop {
            let hits = match vindex.search(qv, k) {
                Ok(v) => v,
                Err(e) => {
                    crate::metrics::zvec_failure(crate::metrics::ZvecOp::Search);
                    eprintln!("dmem: vector search failed ({e:#}); using keyword results only");
                    return Ok(Vec::new());
                }
            };
            let exhausted = hits.len() < k;
            let mut vec = self.resolve_vector_hits(hits, entries)?;
            if vec.len() >= pool || exhausted || k >= MAX_VECTOR_SLOTS || !self.store.filtered_reads() {
                vec.truncate(pool);
                return Ok(vec);
            }
            k = (k * 4).min(MAX_VECTOR_SLOTS);
        }
    }

    /// Resolve vector hits to the one record each slot embeds - (uri, scope) - and hydrate it as
    /// THAT record into `entries` (keyed by `record_key`; keyword hits already there are reused),
    /// BEFORE the floor and fusion. Returns the vector channel as (record key, cosine), best
    /// first. A slot of a scope this reader cannot see, or of a record no longer current, drops
    /// out here: another audience's text never ranks a record this reader gets (a room re-saving
    /// a global title used to rank the global record by the room's text), nor sets the relative
    /// floor for it.
    #[cfg(feature = "zvec")]
    fn resolve_vector_hits(
        &self,
        hits: Vec<crate::zvec_index::VectorHit>,
        entries: &mut std::collections::HashMap<String, Entry>,
    ) -> Result<Vec<(String, f32)>> {
        let mut vec: Vec<(String, f32)> = Vec::with_capacity(hits.len());
        let mut live_scopes: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for h in hits {
            if !live_scopes.contains_key(&h.uri) {
                let scopes = self.store.live_scopes_of(&h.uri)?;
                live_scopes.insert(h.uri.clone(), scopes);
            }
            let Some(scope) = live_scopes[&h.uri].iter().find(|s| crate::zvec_index::slot_of(&h.uri, s) == h.slot).cloned()
            else {
                continue; // a stale slot: no live row of that record any more
            };
            let key = record_key(&h.uri, &scope);
            if !entries.contains_key(&key) {
                match self.store.get_in_scope(&h.uri, &scope)? {
                    Some(e) => {
                        entries.insert(key.clone(), e);
                    }
                    None => continue, // invisible to this reader, or not valid now
                }
            }
            vec.push((key, h.score));
        }
        Ok(vec)
    }

    /// Re-rank keyword hits by their FTS order (base = 1/(1+rank)), gently nudged by the
    /// runtime-signal multiplier (<=1.25x). The base dominates at the top, so a clearly
    /// higher-ranked hit is not displaced; adjacent items at deeper ranks may reorder.
    fn rescore_keyword(&self, hits: Vec<Entry>, limit: usize) -> Vec<Entry> {
        let now = now_ms();
        let keys: Vec<String> = hits.iter().map(|e| signal_key(&e.uri, &e.scope)).collect();
        let sigs = self.store.read_signals(&keys).unwrap_or_default();
        let mut scored: Vec<(Entry, f64)> = hits
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let base = 1.0 / (1.0 + i as f64);
                let (ac, la) = sigs.get(&signal_key(&e.uri, &e.scope)).copied().unwrap_or((0, 0));
                let s = base * signal_boost(e.importance, ac, la, now);
                (e, s)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        one_per_uri(scored, self.write_scope.as_deref()).into_iter().take(limit).collect()
    }

    /// Best-effort: bump the access signals for the recalled set in one transaction.
    /// Never fails recall. Per RECORD (`signal_key`): recalling a room's copy of a title bumps
    /// that copy, never the global record under the same uri that other audiences rank by.
    fn bump_recalled(&self, entries: &[Entry]) {
        let keys: Vec<String> = entries.iter().map(|e| signal_key(&e.uri, &e.scope)).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let _ = self.store.bump_signals(&keys, now_ms());
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        self.store.recent(limit)
    }

    /// Bitemporal recall: as the store existed AS OF system-time `as_of_ms`, for facts
    /// VALID AT `valid_ms`. Keyword-only by design (vectors index only the current version).
    pub fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>> {
        self.store.recall_as_of(query, limit, as_of_ms, valid_ms)
    }

    /// Full version lineage of a uri, newest first (append-only history).
    pub fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>> {
        self.store.history(uri, limit)
    }

    /// Retract a uri: drop it from recall (close current version, keep lineage) and drop the
    /// retracted records' vector slots. Scope-exact: a scoped writer closes only the rows in its own write
    /// scope, and the count covers those rows only (another scope's rows under the same uri
    /// are neither closed nor counted). Returns how many live versions were closed.
    pub fn forget(&self, uri: &str) -> Result<usize> {
        self.guard_mutation_target(uri)?;
        #[cfg(feature = "zvec")]
        let slots = self.mutation_slots(uri)?;
        let n = self.store.forget_in(uri, self.mutation_scope()?)?;
        #[cfg(feature = "zvec")]
        self.resync_vector_slots(uri, &slots);
        Ok(n)
    }

    /// Which recall path is active (truthful: reflects whether zvec actually loaded).
    pub fn recall_mode(&self) -> &'static str {
        #[cfg(feature = "zvec")]
        {
            if self.vindex.is_some() {
                "hybrid: SQLite FTS + zvec vector (RRF)"
            } else {
                "keyword only (SQLite FTS; zvec failed to load)"
            }
        }
        #[cfg(not(feature = "zvec"))]
        {
            "keyword only (SQLite FTS)"
        }
    }

    /// Persona + protocol records (the boot layer), most important first. Agent-less callers
    /// (embedded mode, hooks, bootstrap) see the legacy full set.
    pub fn persona(&self) -> Result<Vec<Entry>> {
        self.persona_for(None)
    }

    /// The boot layer VISIBLE TO an agent identity: shared governance (persona/protocol records
    /// outside the `agents/` namespace tree) plus that agent's own `agents/<agent>/...` records;
    /// other agents' personas are excluded, so a token's identity decides which "I am ..." it is
    /// served. `None` = every record (exactly the pre-agent behaviour).
    pub fn persona_for(&self, agent: Option<&str>) -> Result<Vec<Entry>> {
        let mut out = self.store.by_kind_for_agent("persona", agent, 5)?;
        out.extend(self.store.by_kind_for_agent("protocol", agent, 5)?);
        Ok(out)
    }

    /// Open reminders (kind=reminder), most important/recent first. The session-start greet
    /// pulls a few of these; the full backlog is on-demand recall.
    pub fn reminders(&self, limit: usize) -> Result<Vec<Entry>> {
        self.store.by_kind("reminder", limit)
    }

    /// All live skill records (kind=skill), for `dmem skills sync`/`list`.
    pub fn skills_all(&self, limit: usize) -> Result<Vec<Entry>> {
        self.store.by_kind("skill", limit)
    }

    /// System-time of the most recent save (for the save-discipline nudge cadence).
    pub fn latest_save_ms(&self) -> Result<Option<i64>> {
        self.store.latest_save_ms()
    }

    // --- graph layer ---

    pub fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()> {
        self.guard_rel(rel)?;
        self.guard_edge_endpoints(from_uri, to_uri)?;
        self.store.link(from_uri, to_uri, rel)
    }
    /// Remove an edge. Any `rel` is accepted here (no `guard_rel`), so a legacy free-text edge
    /// can still be removed; the endpoint and ownership guard is the same as `link`'s.
    pub fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        self.guard_edge_endpoints(from_uri, to_uri)?;
        self.store.unlink(from_uri, to_uri, rel)
    }

    /// A write-restricted identity may only create edges with a token label (`valid_rel`).
    /// Full-tenant writers are unchanged: older clients and agents on full-tenant tokens keep
    /// whatever labels they already use.
    fn guard_rel(&self, rel: &str) -> Result<()> {
        if !self.write_restricted() {
            return Ok(());
        }
        self.effective_write_scope()?; // a write-denied identity hears that reason first
        if !valid_rel(rel) {
            return refuse("invalid rel: use one lowercase token (a letter, then a-z, 0-9, _ or -; at most 32 characters)");
        }
        Ok(())
    }

    /// Edge mutations for a restricted identity require BOTH endpoints to be live and visible
    /// (edges carry no content, so read visibility is the right bar - a scoped principal may
    /// link its own record to a global entity, but never touch edges of records it cannot see).
    /// A write-restricted identity also needs ONE endpoint that is wholly its own: every live
    /// row of that uri in its write scope. Otherwise a scoped writer could wire (or cut) edges
    /// between records other audiences own - two global records, or a uri another scope holds
    /// too - and every reader of those records would see the result. Visibility is checked
    /// first, so an invisible endpoint reads as absent, never as "not yours".
    fn guard_edge_endpoints(&self, from_uri: &str, to_uri: &str) -> Result<()> {
        if !self.write_restricted() && self.agent_view.is_none() {
            return Ok(());
        }
        let ws = self.effective_write_scope()?; // write-denied identities cannot mutate edges either
        for u in [from_uri, to_uri] {
            if self.store.get(u)?.is_none() {
                return refuse(format!("edge endpoint not found (or not visible to this identity): {u}"));
            }
        }
        if self.write_restricted() && !self.store.wholly_in_scope(from_uri, ws)? && !self.store.wholly_in_scope(to_uri, ws)? {
            return refuse("an edge needs one endpoint that only this identity's write scope holds");
        }
        Ok(())
    }
    /// Batch graph maintenance (`reindex_links`, `reindex_mentions`) is for a full-tenant
    /// identity only, on every surface (REST refuses scoped tokens up front; MCP reaches the
    /// engine directly, so the rule lives here). It writes edges with no endpoint or ownership
    /// check and prunes dangling edges tenant-wide, so run by anyone else it would wire records
    /// of other audiences together, write for a read-only token, and count edges the caller
    /// cannot see. A write-denied identity (a read-only token, or a full-tenant token that
    /// named no scope on a tenant that requires one) hears that reason first.
    fn guard_batch_graph_maintenance(&self) -> Result<()> {
        self.effective_write_scope()?;
        if !self.full_tenant() {
            return refuse("batch graph maintenance needs a full-tenant token writing global");
        }
        Ok(())
    }

    pub fn edges_of(&self, uri: &str) -> Result<Vec<Edge>> {
        self.store.edges_of(uri)
    }
    pub fn all_edges(&self, limit: usize) -> Result<Vec<Edge>> {
        self.store.all_edges(limit)
    }
    pub fn neighbors(&self, seeds: &[String], depth: usize, limit: usize) -> Result<Vec<String>> {
        self.store.neighbors(seeds, depth, limit)
    }

    /// Graph-augmented recall: find seeds by content, then pull their bounded-hop neighborhood and
    /// hydrate it, so connected-but-not-similar records ride along. Seeds first, then neighbors.
    pub fn recall_expanded(&self, query: &str, limit: usize, depth: usize) -> Result<Vec<Entry>> {
        let (mut seeds, neighbors) = self.recall_expanded_split(query, limit, depth)?;
        seeds.extend(neighbors);
        Ok(seeds)
    }

    /// Graph-augmented recall, split: the content-matched seeds and their live N-hop
    /// neighborhood as separate lists, so callers can render the graph's contribution
    /// distinguishably. Thin wrapper over `recall_expanded_graph` for callers that only
    /// want the entries.
    pub fn recall_expanded_split(&self, query: &str, limit: usize, depth: usize) -> Result<(Vec<Entry>, Vec<Entry>)> {
        let g = self.recall_expanded_graph(query, limit, depth)?;
        Ok((g.seeds, g.riders.into_iter().map(|r| r.entry).collect()))
    }

    /// Graph-augmented recall with the graph made visible: seeds are content matches; riders
    /// are their bounded-hop neighborhood ranked by `seed_weight * decay^hop` (best-scoring
    /// arrival path, see `MemoryStore::neighbors_scored`) instead of BFS arrival order, each
    /// carrying its provenance (hop, via, rel, score); `links` is the mini-subgraph - every
    /// edge whose BOTH endpoints made the result set - so a consumer can see how the results
    /// relate without another round-trip.
    ///
    /// Seed weights decay with recall rank (1, 1/2, 1/3, ...): a rider adjacent to the top
    /// hit outranks one hanging off the sixth. The recall channel is already rank-ordered and
    /// `Entry` carries no score, so rank is the honest signal available. Rider slots fill from
    /// an over-fetched candidate pool: edges are non-cascading (a forgotten endpoint keeps its
    /// edges), so dead URIs are skipped WITHOUT consuming the cap, and `kind=skill` records
    /// never ride recall (they surface only via the skills projection - same invariant as the
    /// keyword and vector channels).
    pub fn recall_expanded_graph(&self, query: &str, limit: usize, depth: usize) -> Result<RecallGraph> {
        let seeds = self.recall(query, limit)?;
        if depth == 0 || seeds.is_empty() {
            return Ok(RecallGraph { seeds, riders: Vec::new(), links: Vec::new() });
        }
        let weighted: Vec<(String, f64)> =
            seeds.iter().enumerate().map(|(i, e)| (e.uri.clone(), 1.0 / (1.0 + i as f64))).collect();
        let hits = self.store.neighbors_scored(
            &weighted,
            depth,
            limit.saturating_mul(4),
            config::recall_decay(),
        )?;
        let mut riders: Vec<RecallRider> = Vec::new();
        for h in hits {
            if riders.len() >= limit {
                break;
            }
            if let Some(e) = self.store.get(&h.uri)? {
                if e.kind == Kind::Skill {
                    continue;
                }
                riders.push(RecallRider { entry: e, hop: h.hop, via: h.via, rel: h.rel, score: h.score });
            }
        }
        let links = self.links_within(&seeds, &riders)?;
        Ok(RecallGraph { seeds, riders, links })
    }

    /// Edges internal to the result set (both endpoints present), deduped, capped. The cap is
    /// a context-budget guard, not a correctness bound: the mini-subgraph is a navigation aid.
    fn links_within(&self, seeds: &[Entry], riders: &[RecallRider]) -> Result<Vec<Edge>> {
        const LINKS_CAP: usize = 50;
        let in_set: std::collections::HashSet<&str> = seeds
            .iter()
            .map(|e| e.uri.as_str())
            .chain(riders.iter().map(|r| r.entry.uri.as_str()))
            .collect();
        let mut seen: std::collections::HashSet<(String, String, String)> = std::collections::HashSet::new();
        let mut links: Vec<Edge> = Vec::new();
        'outer: for uri in &in_set {
            for e in self.store.edges_of(uri)? {
                if !in_set.contains(e.from_uri.as_str()) || !in_set.contains(e.to_uri.as_str()) {
                    continue;
                }
                if seen.insert((e.from_uri.clone(), e.to_uri.clone(), e.rel.clone())) {
                    links.push(e);
                    if links.len() >= LINKS_CAP {
                        break 'outer;
                    }
                }
            }
        }
        // Deterministic order regardless of HashSet iteration.
        links.sort_by(|a, b| a.from_uri.cmp(&b.from_uri).then(a.to_uri.cmp(&b.to_uri)).then(a.rel.cmp(&b.rel)));
        Ok(links)
    }

    /// Rebuild edges from the `[[name]]` references in every current record's body. Batch, not
    /// on-save, so writes stay fast at scale: build a slug->uri map once (the slug is the uri's
    /// last segment), then resolve each `[[name]]` against it in memory. Idempotent. Returns the
    /// count of `[[name]]` references that resolved to a record and were linked.
    pub fn reindex_links(&self) -> Result<(usize, usize)> {
        self.guard_batch_graph_maintenance()?;
        // Hygiene first: clear edges whose endpoint is forgotten or never existed, so stale
        // relations do not survive the rebuild (legitimate ones are re-derived right after).
        let pruned = self.store.prune_dangling_edges()?;
        let records = self.store.recent(1_000_000)?;
        // recent() is ordered importance DESC then created DESC, so on a slug collision or_insert
        // keeps the highest-importance (then newest) record as the link target. Deterministic.
        let mut by_slug: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for e in &records {
            if let Some(slug) = e.uri.rsplit('/').next() {
                by_slug.entry(slug.to_string()).or_insert_with(|| e.uri.clone());
            }
        }
        let live_uris: std::collections::HashSet<&str> = records.iter().map(|e| e.uri.as_str()).collect();
        let mut linked = 0usize;
        for e in &records {
            for name in parse_wikilinks(&e.body) {
                // Resolution ladder : a direct [[daimon://...]] uri
                // reference resolves exactly when that record is live; everything else goes
                // through the title slug (slug() lowercases, so title matching is already
                // case-insensitive).
                let target: Option<String> = if name.starts_with("daimon://") {
                    live_uris.contains(name.as_str()).then(|| name.clone())
                } else {
                    let slug = crate::entry::slug(&name);
                    if slug.is_empty() {
                        continue;
                    }
                    by_slug.get(&slug).cloned()
                };
                if let Some(target) = target {
                    if target != e.uri {
                        self.store.link(&e.uri, &target, "links")?;
                        linked += 1;
                    }
                }
            }
        }
        Ok((linked, pruned))
    }

    /// Deterministic entity-mention pass: link `record -[mentions]-> entity` wherever a record's
    /// title or body mentions a canonical entity title in plain text (word-boundary, exact case -
    /// the conservative pass). The `mentions` rel is distinct from the curated `links` rel, so
    /// expansion can weight curated edges above mined ones and this whole pass can be retracted
    /// without touching hand-made edges. Skips pairs already connected by any rel in either
    /// direction. Idempotent; `dry_run` counts without writing. Returns
    /// (mention pairs found, edges added - or would be added under dry_run).
    pub fn reindex_mentions(&self, dry_run: bool) -> Result<(usize, usize)> {
        self.guard_batch_graph_maintenance()?;
        let records = self.store.recent(1_000_000)?;
        let entities: Vec<(&Entry, Vec<String>)> =
            records.iter().filter(|e| e.kind.is_entity()).map(|e| (e, entity_aliases(&e.title))).collect();
        let mut have: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for e in self.store.all_edges(1_000_000)? {
            have.insert((e.from_uri.clone(), e.to_uri.clone()));
            have.insert((e.to_uri, e.from_uri));
        }
        let (mut found, mut added) = (0usize, 0usize);
        for rec in &records {
            let text = format!("{}\n{}", rec.title, rec.body);
            for (ent, aliases) in &entities {
                if ent.uri == rec.uri {
                    continue;
                }
                if aliases.iter().any(|a| mentions_word(&text, a)) {
                    found += 1;
                    if !have.contains(&(rec.uri.clone(), ent.uri.clone())) {
                        if !dry_run {
                            self.store.link(&rec.uri, &ent.uri, "mentions")?;
                        }
                        have.insert((rec.uri.clone(), ent.uri.clone()));
                        have.insert((ent.uri.clone(), rec.uri.clone()));
                        added += 1;
                    }
                }
            }
        }
        Ok((found, added))
    }

    /// Construct a LocalMemory directly over a store (tests only; bypasses config). Uses the
    /// cheap HashEmbedder rather than `make_embedder` so tests never load a real model (no
    /// network, fast, deterministic); `vindex: None` keeps recall on the keyword path.
    #[cfg(test)]
    pub(crate) fn for_test(store: SqliteStore) -> Self {
        #[cfg(feature = "zvec")]
        {
            Self {
                store,
                write_scope: Some(String::new()),
                agent_view: None,
                write_refusal: None,
                scope_unbound: false,
                vindex: None,
                embedder: std::sync::Arc::new(crate::embedder::HashEmbedder::new()),
                pre: PreEmbedded::default(),
                engine_embeds: Default::default(),
            }
        }
        #[cfg(not(feature = "zvec"))]
        {
            Self { store, write_scope: Some(String::new()), agent_view: None, write_refusal: None, scope_unbound: false }
        }
    }

    /// As `for_test`, plus a real on-disk vector index in `vdir` (hybrid recall and index maintenance
    /// under test), still on the deterministic HashEmbedder.
    #[cfg(all(test, feature = "zvec"))]
    /// Opens through the production path (`open_vector_index`), so an existing pre-slot index in
    /// `vdir` is migrated exactly as at a real start-up.
    pub(crate) fn for_test_vec(store: SqliteStore, vdir: &std::path::Path) -> Self {
        let mut m = Self::for_test(store);
        m.vindex = Some(std::sync::Arc::new(open_vector_index(&m.store, vdir, &*m.embedder, false, false).unwrap().expect("open test vector index")));
        m
    }
}

/// The memory handle callers use: either the local engine or a remote `dmem serve` client,
/// chosen at `open()` by whether a `[server]` block is configured. The two modes share the
/// same surface, so callers (CLI, hooks, MCP) are mode-agnostic.
pub enum Memory {
    Local(LocalMemory),
    #[cfg(feature = "client")]
    Remote(crate::client::RemoteClient),
}

impl Memory {
    /// Remote-client if a `[server]` block is configured (and the client feature is built),
    /// else the local embedded engine.
    pub fn open() -> Result<Self> {
        #[cfg(feature = "client")]
        if let Some(link) = config::server_link() {
            let client = crate::client::RemoteClient::new(link)?.with_write_scope(config::write_scope())?;
            return Ok(Memory::Remote(client));
        }
        Ok(Memory::Local(LocalMemory::open()?))
    }

    /// Open a specific LOCAL tenant (the server is always local-backed; never remote).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub fn open_tenant(tenant: &str) -> Result<LocalMemory> {
        LocalMemory::open_tenant(tenant)
    }

    /// Borrow the inner local engine (tests only; the skills tests import through `Memory` then
    /// read back through the local store).
    #[cfg(test)]
    pub(crate) fn as_local(&self) -> &LocalMemory {
        match self {
            Memory::Local(l) => l,
            #[cfg(feature = "client")]
            Memory::Remote(_) => panic!("as_local called on a remote Memory"),
        }
    }

    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.recall(query, limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall(query, limit),
        }
    }
    pub fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.recall_as_of(query, limit, as_of_ms, valid_ms),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall_as_of(query, limit, as_of_ms, valid_ms),
        }
    }
    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.recent(limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recent(limit),
        }
    }
    pub fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.history(uri, limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.history(uri, limit),
        }
    }
    pub fn forget(&self, uri: &str) -> Result<usize> {
        match self {
            Memory::Local(l) => l.forget(uri),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.forget(uri),
        }
    }
    pub fn persona(&self) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.persona(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.persona(),
        }
    }
    pub fn reminders(&self, limit: usize) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.reminders(limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.reminders(limit),
        }
    }
    pub fn latest_save_ms(&self) -> Result<Option<i64>> {
        match self {
            Memory::Local(l) => l.latest_save_ms(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.latest_save_ms(),
        }
    }
    pub fn counts(&self) -> Result<Vec<(String, usize)>> {
        match self {
            Memory::Local(l) => l.counts(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.counts(),
        }
    }
    pub fn recall_mode(&self) -> &'static str {
        match self {
            Memory::Local(l) => l.recall_mode(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall_mode(),
        }
    }
    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>) -> Result<String> {
        match self {
            // Embedded mode carries no agent identity; attribution is a server-side concern
            // (the remote path is stamped by the server from the token).
            Memory::Local(l) => l.remember(text, namespace, valid_from, valid_to, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.remember(text, namespace, valid_from, valid_to),
        }
    }
    pub fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        match self {
            Memory::Local(l) => l.invalidate(uri, valid_to_ms),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.invalidate(uri, valid_to_ms),
        }
    }
    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_decision(title, context, decision, rationale, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_decision(title, context, decision, rationale, namespace),
        }
    }
    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_lesson(title, lesson, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_lesson(title, lesson, namespace),
        }
    }
    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_incident(title, impact, resolution, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_incident(title, impact, resolution, namespace),
        }
    }
    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.add_reminder(title, text, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.add_reminder(title, text, namespace),
        }
    }
    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_runbook(title, steps, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_runbook(title, steps, namespace),
        }
    }
    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_convention(title, rule, namespace, None),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_convention(title, rule, namespace),
        }
    }
    pub fn import_record(&self, kind: Kind, namespace: &str, title: &str, body: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.import_record(kind, namespace, title, body),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.import_record(kind, namespace, title, body),
        }
    }
    pub fn import_record_at(&self, kind: Kind, namespace: &str, title: &str, body: &str, created_ms: i64, importance: Option<i64>) -> Result<String> {
        match self {
            Memory::Local(l) => l.import_record_at(kind, namespace, title, body, created_ms, importance),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.import_record_at(kind, namespace, title, body, created_ms, importance),
        }
    }
    pub fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()> {
        match self {
            Memory::Local(l) => l.link(from_uri, to_uri, rel),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.link(from_uri, to_uri, rel),
        }
    }
    pub fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        match self {
            Memory::Local(l) => l.unlink(from_uri, to_uri, rel),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.unlink(from_uri, to_uri, rel),
        }
    }
    pub fn edges_of(&self, uri: &str) -> Result<Vec<Edge>> {
        match self {
            Memory::Local(l) => l.edges_of(uri),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.edges_of(uri),
        }
    }
    pub fn all_edges(&self, limit: usize) -> Result<Vec<Edge>> {
        match self {
            Memory::Local(l) => l.all_edges(limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.all_edges(limit),
        }
    }
    pub fn neighbors(&self, seeds: &[String], depth: usize, limit: usize) -> Result<Vec<String>> {
        match self {
            Memory::Local(l) => l.neighbors(seeds, depth, limit),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.neighbors(seeds, depth, limit),
        }
    }
    pub fn recall_expanded(&self, query: &str, limit: usize, depth: usize) -> Result<Vec<Entry>> {
        match self {
            Memory::Local(l) => l.recall_expanded(query, limit, depth),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall_expanded(query, limit, depth),
        }
    }
    pub fn recall_expanded_split(&self, query: &str, limit: usize, depth: usize) -> Result<(Vec<Entry>, Vec<Entry>)> {
        match self {
            Memory::Local(l) => l.recall_expanded_split(query, limit, depth),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall_expanded_split(query, limit, depth),
        }
    }
    pub fn recall_expanded_graph(&self, query: &str, limit: usize, depth: usize) -> Result<RecallGraph> {
        match self {
            Memory::Local(l) => l.recall_expanded_graph(query, limit, depth),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.recall_expanded_graph(query, limit, depth),
        }
    }
    pub fn reindex_links(&self) -> Result<(usize, usize)> {
        match self {
            Memory::Local(l) => l.reindex_links(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.reindex_links(),
        }
    }

    pub fn reindex_mentions(&self, dry_run: bool) -> Result<(usize, usize)> {
        match self {
            Memory::Local(l) => l.reindex_mentions(dry_run),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.reindex_mentions(dry_run),
        }
    }
    /// Re-embed every live record into the local vector index (embedded mode only; the work is
    /// local embedding + upsert, so it is not exposed over the remote client).
    #[cfg(feature = "zvec")]
    pub fn reindex_embeddings(&self) -> Result<(usize, usize)> {
        match self {
            Memory::Local(l) => l.reindex_embeddings(),
            #[cfg(feature = "client")]
            Memory::Remote(_) => anyhow::bail!(
                "reindex-embeddings runs in embedded mode against the local store, not over a remote client"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_store() -> SqliteStore {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmtools-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        SqliteStore::open(&dir.join("t.db")).unwrap()
    }

    fn ent(uri: &str, title: &str) -> Entry {
        Entry::new_now(uri.into(), Kind::Memory, "ns".into(), title.into(), "".into(), vec![], 50, uri.into())
    }

    #[test]
    fn signal_boost_is_modest_and_monotonic() {
        let day = 86_400_000i64;
        let low = signal_boost(50, 0, 0, day);
        let high = signal_boost(90, 50, day, day);
        assert!(high > low, "more importance/access/recency must boost more");
        assert!(low >= 1.0 && high <= 1.25, "boost clamped to [1.0,1.25]: low={low} high={high}");
    }

    #[test]
    fn clearly_stronger_relevance_is_preserved() {
        let store = tmp_store();
        // hammer the access signal of a DEEPER hit (rank 5) - the bounded (<=1.25x) boost
        // must still not lift it past the clearly higher-ranked hit at rank 0.
        for _ in 0..1000 {
            store.bump_signal("daimon://freq", now_ms()).unwrap();
        }
        let m = LocalMemory::for_test(store);
        let hits = vec![
            ent("daimon://strong", "Strong"), // rank 0 (base 1.0)
            ent("daimon://h1", "h1"),
            ent("daimon://h2", "h2"),
            ent("daimon://h3", "h3"),
            ent("daimon://h4", "h4"),
            ent("daimon://freq", "Freq"), // rank 5 (base 1/6), heavily accessed
        ];
        let out = m.rescore_keyword(hits, 10);
        assert_eq!(
            out[0].uri, "daimon://strong",
            "a clearly higher-ranked hit must not be displaced by a deeper, much-accessed one"
        );
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn stamp_author_sets_once_and_never_reassigns() {
        let mut tags = vec!["decision".to_string()];
        stamp_author(&mut tags, Some("assistant"));
        assert!(tags.contains(&"author:assistant".to_string()));
        // a later write path with a different agent must not re-assign attribution
        stamp_author(&mut tags, Some("devin"));
        assert_eq!(tags.iter().filter(|t| t.starts_with("author:")).count(), 1);
        assert!(tags.contains(&"author:assistant".to_string()), "first attribution wins");
        // agent-less: byte-identical tags
        let mut plain = vec!["reminder".to_string()];
        stamp_author(&mut plain, None);
        assert_eq!(plain, vec!["reminder".to_string()]);
    }

    #[test]
    fn authenticated_agent_writes_carry_author_attribution() {
        let m = LocalMemory::for_test(tmp_store());
        let uri = m.log_decision("Pick zvec", "", "zvec it is", "", "resources/notes", Some("assistant")).unwrap();
        let hit = m.recall("pick zvec", 5).unwrap().into_iter().find(|e| e.uri == uri).expect("recalled");
        assert!(hit.tags.contains(&"author:assistant".to_string()), "typed save stamps the author: {:?}", hit.tags);

        let uri = m.remember("the bridge runs on kestrel", "resources/notes", None, None, Some("analyst")).unwrap();
        let hit = m.recall("bridge kestrel", 5).unwrap().into_iter().find(|e| e.uri == uri).expect("recalled");
        assert!(hit.tags.contains(&"author:analyst".to_string()), "free-form save stamps the author");

        // agent-less write: no author tag, exactly as before
        let uri = m.add_reminder("check backups", "verify k10 export", "agent/reminders", None).unwrap();
        let hit = m.reminders(5).unwrap().into_iter().find(|e| e.uri == uri).expect("listed");
        assert!(!hit.tags.iter().any(|t| t.starts_with("author:")), "agent-less write stays unstamped");
    }

    #[test]
    fn persona_for_serves_own_identity_plus_shared_governance() {
        let m = LocalMemory::for_test(tmp_store());
        m.import_record(Kind::Persona, "shared/governance", "House Rules", "shared boundaries").unwrap();
        m.import_record(Kind::Persona, "agents/assistant/persona", "Assistant Persona", "I am the assistant").unwrap();
        m.import_record(Kind::Persona, "agents/analyst/persona", "Analyst Persona", "I am the analyst").unwrap();
        m.import_record(Kind::Protocol, "agent/protocol", "Behavioral Discipline", "recall before reasoning").unwrap();

        let assistant: Vec<String> = m.persona_for(Some("assistant")).unwrap().into_iter().map(|e| e.title).collect();
        assert!(assistant.contains(&"Assistant Persona".to_string()), "own persona served");
        assert!(assistant.contains(&"House Rules".to_string()), "shared governance served");
        assert!(assistant.contains(&"Behavioral Discipline".to_string()), "protocols are shared");
        assert!(!assistant.contains(&"Analyst Persona".to_string()), "another agent's persona never leaks");

        // agent-less: the legacy full set (backward compatibility until migration)
        let all: Vec<String> = m.persona().unwrap().into_iter().map(|e| e.title).collect();
        assert!(all.contains(&"Assistant Persona".to_string()) && all.contains(&"Analyst Persona".to_string()));
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn remember_valid_and_invalidate_wire_through_the_api() {
        let m = LocalMemory::for_test(tmp_store());
        let uri = m.remember("status is green", "resources/notes", Some(100), None, None).unwrap();
        assert_eq!(m.invalidate(&uri, 300).unwrap(), 1, "one segment invalidated");
        assert!(m.recall("green", 5).unwrap().is_empty(), "no longer valid now");
        let past = m.recall_as_of("green", 5, now_ms(), 200).unwrap();
        assert!(past.iter().any(|e| e.body.contains("green")), "valid-as-of 200 still sees it");
    }

    #[test]
    fn reindex_links_resolves_wikilinks_and_recall_expands() {
        let m = LocalMemory::for_test(tmp_store());
        m.remember("Beta the target", "resources/notes", None, None, None).unwrap();
        m.remember("Alpha refers to [[Beta the target]] for context", "resources/notes", None, None, None).unwrap();
        let (n, _pruned) = m.reindex_links().unwrap();
        assert!(n >= 1, "the [[Beta the target]] reference should resolve and link");
        // a query that only hits alpha still pulls beta in, via the edge
        let hits = m.recall_expanded("Alpha refers context", 3, 1).unwrap();
        assert!(hits.iter().any(|e| e.body.contains("Beta the target")), "neighbor pulled in via the graph");
    }

    #[test]
    fn reindex_mentions_links_plaintext_entity_mentions() {
        let m = LocalMemory::for_test(tmp_store());
        let kestrel = m.import_record(Kind::Site, "resources/entities", "kestrel",
            &entity_body(Kind::Site, "kestrel", &[], "Lab node")).unwrap();
        let desk = m.import_record(Kind::Site, "resources/entities", "Larkdesk (Windows)",
            &entity_body(Kind::Site, "Larkdesk (Windows)", &[], "Workstation")).unwrap();
        let hit = m.remember("Rebooted kestrel after the NFS mount hung", "resources/notes", None, None, None).unwrap();
        m.remember("Vectors and kestreld are not mentions of the node", "resources/notes", None, None, None).unwrap();
        let cased = m.remember("KESTREL in caps must not match the conservative pass", "resources/notes", None, None, None).unwrap();
        let alias = m.remember("Provisioned Larkdesk with Claude Code", "resources/notes", None, None, None).unwrap();
        // dry run counts but writes nothing
        let (found_dry, added_dry) = m.reindex_mentions(true).unwrap();
        assert!(added_dry >= 2, "dry run sees the candidates");
        assert!(m.edges_of(&hit).unwrap().is_empty(), "dry run must not write edges");
        // real pass links exactly the word-boundary, exact-case mentions (alias via parenthetical strip)
        let (found, added) = m.reindex_mentions(false).unwrap();
        assert_eq!((found, added), (found_dry, added_dry), "dry run and real pass agree");
        assert!(m.edges_of(&hit).unwrap().iter().any(|e| e.to_uri == kestrel && e.rel == "mentions"));
        assert!(m.edges_of(&alias).unwrap().iter().any(|e| e.to_uri == desk), "parenthetical-stripped alias resolves");
        assert!(m.edges_of(&cased).unwrap().is_empty(), "exact case only: KESTREL does not match kestrel");
        // idempotent: a second pass adds nothing
        let (_, again) = m.reindex_mentions(false).unwrap();
        assert_eq!(again, 0, "second pass is a no-op");
    }

    #[test]
    fn expanded_split_skips_skills_and_dead_endpoints() {
        let m = LocalMemory::for_test(tmp_store());
        let alpha = m.remember("Alpha hub links to everything relevant", "resources/notes", None, None, None).unwrap();
        let live = m.remember("Beta the live neighbor rides along", "resources/notes", None, None, None).unwrap();
        let skill = m.import_record(Kind::Skill, "agent/skills", "Secret skill", "skill body must never ride recall").unwrap();
        m.link(&alpha, &live, "mentions").unwrap();
        m.link(&alpha, &skill, "mentions").unwrap();
        // edges are non-cascading: a dead endpoint keeps its edge but must not consume slots
        m.link(&alpha, "daimon://resources/notes/memory/long-forgotten", "mentions").unwrap();
        let (seeds, neighbors) = m.recall_expanded_split("Alpha hub relevant", 3, 1).unwrap();
        assert!(seeds.iter().any(|e| e.uri == alpha), "seed found by content");
        assert!(neighbors.iter().any(|e| e.uri == live), "live neighbor hydrated");
        assert!(neighbors.iter().all(|e| e.kind != Kind::Skill), "skills never ride recall (invariant)");
        assert!(neighbors.iter().all(|e| e.uri != "daimon://resources/notes/memory/long-forgotten"), "dead endpoint dropped");
    }

    #[test]
    fn wikilinks_pipe_alias_and_direct_uri_resolve() {
        // parse: the pipe alias form yields the TARGET, not the shown text
        assert_eq!(parse_wikilinks("see [[Real Target|the shown words]] here"), vec!["Real Target"]);
        assert_eq!(parse_wikilinks("[[Plain]] and [[A|b]] and [[]]"), vec!["Plain", "A"]);
        // reindex: title-slug (case handled by slug()), pipe alias, and direct daimon:// uri
        let m = LocalMemory::for_test(tmp_store());
        let beta = m.remember("Beta Target Record", "resources/notes", None, None, None).unwrap();
        m.remember(&format!("Alpha refers to [[beta target record|itu]] and [[{beta}]] twice"), "resources/notes", None, None, None)
            .unwrap();
        let (linked, _pruned) = m.reindex_links().unwrap();
        assert!(linked >= 2, "pipe alias and direct uri both resolve: {linked}");
        let hits = m.recall_expanded("Alpha refers twice", 3, 1).unwrap();
        assert!(hits.iter().any(|e| e.uri == beta), "neighbor rides in via the resolved edges");
    }

    #[test]
    fn expanded_graph_ranks_riders_by_decayed_score_not_arrival_order() {
        // seed -> near (hop 1) -> far (hop 2): with one rider slot, hop-decay must pick `near`
        // even though the flat BFS pool contains both.
        let m = LocalMemory::for_test(tmp_store());
        let seed = m.remember("Gamma anchor matches the query text", "resources/notes", None, None, None).unwrap();
        let near = m.remember("Near rider one hop out", "resources/notes", None, None, None).unwrap();
        let far = m.remember("Far rider two hops out", "resources/notes", None, None, None).unwrap();
        m.link(&near, &far, "links").unwrap(); // inserted before the seed edge: arrival order must not matter
        m.link(&seed, &near, "links").unwrap();
        let g = m.recall_expanded_graph("Gamma anchor matches", 1, 2).unwrap();
        assert_eq!(g.riders.len(), 1, "rider slots honour the limit");
        let r = &g.riders[0];
        assert_eq!(r.entry.uri, near, "hop-1 outranks hop-2");
        assert_eq!((r.hop, r.via.as_str(), r.rel.as_str()), (1, seed.as_str(), "links"), "provenance points at the seed");
        assert!(r.score > 0.0, "rider carries a decayed score");
    }

    #[test]
    fn scope_context_stamps_writes_and_narrows_reads() {
        let mut m = LocalMemory::for_test(tmp_store());
        let global = m.remember("Team runbook everyone may read", "resources/notes", None, None, None).unwrap();
        m.set_scope_context(Some("user:a"), Some(vec!["user:a".to_string()]));
        let mine = m.remember("Private draft plan for later", "resources/notes", None, None, None).unwrap();
        let mine_e = m.store.get(&mine).unwrap().expect("own record visible");
        assert_eq!(mine_e.scope, "user:a", "write stamped with the identity's scope, not caller input");
        let recent = m.recent(10).unwrap();
        assert!(recent.iter().any(|e| e.uri == global), "global rides along");
        assert!(recent.iter().any(|e| e.uri == mine), "own scope visible");
        // switch reader to a different principal: the other scope reads as absent everywhere
        m.set_scope_context(Some("user:b"), Some(vec!["user:b".to_string()]));
        assert!(m.store.get(&mine).unwrap().is_none());
        assert!(m.recall("private draft plan", 5).unwrap().iter().all(|e| e.uri != mine));
        // a scoped principal is a user, not an agent: the agents/ tree is off-limits even
        // without an agent label in play (0.3.2 live-smoke catch: persona poison path)
        assert!(
            m.import_record(Kind::Persona, "agents/assistant", "poison", "planted persona").is_err(),
            "scoped identities cannot write into the agents/ tree"
        );
    }

    #[test]
    fn agent_view_guards_the_agents_tree_on_every_surface() {
        // Audit High #1: an agent identity must not read, write, or retract another agent's
        // agents/<other>/... records through ANY surface - not just the persona route.
        let mut m = LocalMemory::for_test(tmp_store());
        let analyst_persona = m
            .import_record(Kind::Persona, "agents/analyst", "Analyst persona", "I am the analyst, the document specialist")
            .unwrap();
        let shared = m.remember("Shared brain fact for everyone", "resources/notes", None, None, None).unwrap();

        m.set_agent_view(Some("assistant".into()));
        // reads: foreign persona invisible everywhere
        assert!(m.recall("I am the analyst specialist", 5).unwrap().iter().all(|e| e.uri != analyst_persona));
        assert!(m.recent(50).unwrap().iter().all(|e| e.uri != analyst_persona));
        assert!(m.history(&analyst_persona, 10).unwrap().is_empty(), "history gated too");
        assert!(m.store.get(&analyst_persona).unwrap().is_none(), "get() reads it as absent");
        // shared pool stays fully visible (the shared brain is the point)
        assert!(m.recent(50).unwrap().iter().any(|e| e.uri == shared));
        // writes into the foreign tree are rejected; own tree + shared are fine
        assert!(m.import_record(Kind::Persona, "agents/analyst", "Fake", "overwrite attempt").is_err());
        assert!(m.import_record(Kind::Persona, "agents/assistant", "Assistant persona", "own tree ok").is_ok());
        assert!(m.remember("shared write ok", "resources/notes", None, None, None).is_ok());
        // retractions of the foreign record are rejected (reads as not-found)
        assert!(m.forget(&analyst_persona).is_err());
        assert!(m.invalidate(&analyst_persona, crate::entry::now_ms() + 10).is_err());
        // agent-less view: everything back to legacy behavior
        m.set_agent_view(None);
        assert!(m.store.get(&analyst_persona).unwrap().is_some());
    }

    #[test]
    fn agent_tree_guard_covers_edges_of_and_case_variants() {
        let mut m = LocalMemory::for_test(tmp_store());
        let analyst = m
            .import_record(Kind::Persona, "agents/analyst", "Analyst persona", "I am the analyst")
            .unwrap();
        let shared = m.remember("Shared record with an edge", "resources/notes", None, None, None).unwrap();
        m.link(&shared, &analyst, "about").unwrap();

        m.set_agent_view(Some("assistant".into()));
        // 2nd-opinion follow-up: /edges must not enumerate a foreign persona tree - neither
        // by querying the foreign uri directly (predictable path) nor via a shared record's
        // edge list leaking the foreign endpoint's slug.
        assert!(m.edges_of(&analyst).unwrap().is_empty(), "probing the foreign uri yields nothing");
        assert!(
            m.edges_of(&shared).unwrap().iter().all(|e| e.from_uri != analyst && e.to_uri != analyst),
            "a shared record's edges hide invisible endpoints"
        );
        // case-variant spelling is the SAME protected tree, not shared pool
        assert!(
            m.import_record(Kind::Persona, "Agents/Analyst", "sneaky", "case bypass attempt").is_err(),
            "Agents/Analyst writes are guarded like agents/analyst"
        );
        assert!(m.import_record(Kind::Persona, "Agents/ASSISTANT", "own tree, odd case", "fine").is_ok());
        m.set_agent_view(None);
        assert_eq!(m.edges_of(&analyst).unwrap().len(), 1, "unrestricted view unchanged");
    }

    // --- E1: scope-exact forget / invalidate through the engine guard ---

    const SHARED_NS: &str = "resources/workspace";
    const SHARED_TITLE: &str = "Shared runbook";

    /// Save the shared-title runbook as `scope` (write scope = read scope = `scope`; "" = the
    /// full-tenant writer). Every call targets the SAME uri - only the scope differs.
    fn save_as(m: &mut LocalMemory, scope: &str, body: &str) -> String {
        if scope.is_empty() {
            m.set_scope_context(Some(""), None);
        } else {
            m.set_scope_context(Some(scope), Some(vec![scope.to_string()]));
        }
        m.import_record(Kind::Runbook, SHARED_NS, SHARED_TITLE, body).unwrap()
    }

    fn as_scope(m: &mut LocalMemory, scope: &str) {
        m.set_scope_context(Some(scope), Some(vec![scope.to_string()]));
    }

    fn as_full(m: &mut LocalMemory) {
        m.set_scope_context(Some(""), None);
    }

    /// A global anchor, linked from `uri` by the full-tenant writer.
    fn link_anchor(m: &mut LocalMemory, uri: &str) -> String {
        as_full(m);
        let anchor = m.import_record(Kind::Org, "resources/entities", "Anchor org", "global anchor").unwrap();
        m.link(uri, &anchor, "about").unwrap();
        anchor
    }

    #[test]
    fn scoped_forget_is_scope_exact_whoever_saved_last() {
        // Both save orders: the old guard judged the NEWEST current row across all scopes, so
        // it refused room a when room b saved last, and closed BOTH rooms' rows when room a
        // saved last. Now room a closes exactly its own row either way.
        for a_saves_last in [false, true] {
            let mut m = LocalMemory::for_test(tmp_store());
            let uri = if a_saves_last {
                save_as(&mut m, "room:b", "room b notes");
                save_as(&mut m, "room:a", "room a notes")
            } else {
                save_as(&mut m, "room:a", "room a notes");
                save_as(&mut m, "room:b", "room b notes")
            };
            link_anchor(&mut m, &uri);
            as_scope(&mut m, "room:a");
            assert_eq!(m.forget(&uri).unwrap(), 1, "room a closes (and counts) its own row only");
            assert_eq!(m.store.live_scopes(&uri), vec!["room:b"], "room b's record stays live");
            assert_eq!(m.store.raw_edge_count(&uri), 1, "room b still holds the node: edges stay");
            // a second forget has nothing of room a's left: refused, not a silent 0
            assert!(m.forget(&uri).is_err());
            // room b's own forget takes the last live row, and the edges with it
            as_scope(&mut m, "room:b");
            assert_eq!(m.forget(&uri).unwrap(), 1);
            assert!(m.store.live_scopes(&uri).is_empty());
            assert_eq!(m.store.raw_edge_count(&uri), 0, "last live row gone: the graph cascades");
        }
    }

    #[test]
    fn scoped_forget_without_own_rows_is_refused() {
        let mut m = LocalMemory::for_test(tmp_store());
        let uri = save_as(&mut m, "", "global notes");
        save_as(&mut m, "room:b", "room b notes");
        as_scope(&mut m, "room:a");
        let err = m.forget(&uri).expect_err("room a holds no row of this uri");
        assert!(err.to_string().contains("not found in this identity's write scope"), "{err}");
        assert_eq!(m.store.live_scopes(&uri), vec!["", "room:b"], "nothing was closed");
        // a write-denied identity cannot retract at all
        m.set_scope_context(None, Some(vec!["room:b".into()]));
        assert!(m.forget(&uri).is_err());
        assert_eq!(m.store.live_scopes(&uri), vec!["", "room:b"]);
    }

    #[test]
    fn full_tenant_forget_is_unchanged() {
        let mut m = LocalMemory::for_test(tmp_store());
        let uri = save_as(&mut m, "", "global notes");
        save_as(&mut m, "room:a", "room a notes");
        link_anchor(&mut m, &uri);
        as_full(&mut m);
        assert_eq!(m.forget(&uri).unwrap(), 2, "full-tenant forget closes every scope's row");
        assert!(m.store.live_scopes(&uri).is_empty());
        assert_eq!(m.store.raw_edge_count(&uri), 0);
        assert_eq!(m.forget(&uri).unwrap(), 0, "nothing live: a no-op, as before");
    }

    #[test]
    fn scoped_forget_retracts_an_invalidated_own_record() {
        // The old guard needed a valid-time CURRENT row, so an owner could not retract its own
        // record once invalidated. Liveness (system time) is the bar now.
        let mut m = LocalMemory::for_test(tmp_store());
        let uri = save_as(&mut m, "room:a", "room a notes");
        as_scope(&mut m, "room:a");
        let from = m.store.newest_live(&uri, None).unwrap().unwrap().valid_from_ms;
        std::thread::sleep(std::time::Duration::from_millis(3)); // now > from + 1: the cut is in the past
        assert_eq!(m.invalidate(&uri, from + 1).unwrap(), 1);
        assert!(m.store.get(&uri).unwrap().is_none(), "no longer current");
        assert_eq!(m.forget(&uri).unwrap(), 1, "still live, still its own: retractable");
    }

    #[test]
    fn scoped_invalidate_is_scope_exact() {
        for a_saves_last in [false, true] {
            let mut m = LocalMemory::for_test(tmp_store());
            let uri = if a_saves_last {
                save_as(&mut m, "room:b", "room b notes");
                save_as(&mut m, "room:a", "room a notes")
            } else {
                save_as(&mut m, "room:a", "room a notes");
                save_as(&mut m, "room:b", "room b notes")
            };
            link_anchor(&mut m, &uri);
            as_scope(&mut m, "room:a");
            let cut = now_ms() + 60_000;
            assert_eq!(m.invalidate(&uri, cut).unwrap(), 1, "room a's segment only");
            let b = m.store.newest_live(&uri, Some("room:b")).unwrap().expect("room b's row stays live");
            assert_eq!(b.valid_to_ms, None, "room b's validity is untouched");
            let a = m.store.newest_live(&uri, Some("room:a")).unwrap().expect("room a keeps a bounded remainder");
            assert_eq!(a.valid_to_ms, Some(cut));
            assert_eq!(m.store.raw_edge_count(&uri), 1, "invalidate never drops a live node's edges");
        }
    }

    #[test]
    fn scoped_invalidate_without_own_rows_is_refused_and_full_tenant_is_unchanged() {
        let mut m = LocalMemory::for_test(tmp_store());
        let uri = save_as(&mut m, "", "global notes");
        save_as(&mut m, "room:b", "room b notes");
        as_scope(&mut m, "room:a");
        let cut = now_ms() + 60_000;
        let err = m.invalidate(&uri, cut).expect_err("room a holds no row of this uri");
        assert!(err.to_string().contains("not found in this identity's write scope"), "{err}");
        as_full(&mut m);
        assert!(m.store.get(&uri).unwrap().is_some_and(|e| e.valid_to_ms.is_none()), "nothing was cut");
        assert_eq!(m.invalidate(&uri, cut).unwrap(), 2, "full-tenant invalidate cuts every scope, as before");
    }

    #[cfg(feature = "zvec")]
    fn tmp_vdir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("dmtools-vec-{}-{}-{}", std::process::id(), now_ms(), n))
    }

    /// Best cosine at which `uri` answers a vector search for `text`, over all of its slots
    /// (None = no vector of the uri in any scope).
    #[cfg(feature = "zvec")]
    fn vec_score(m: &LocalMemory, text: &str, uri: &str) -> Option<f32> {
        let qv = m.embedder.embed(text);
        m.vindex.as_ref().unwrap().search(&qv, 64).unwrap().into_iter().find(|h| h.uri == uri).map(|h| h.score)
    }

    /// As `vec_score`, for the one (uri, scope) record's slot.
    #[cfg(feature = "zvec")]
    fn slot_score(m: &LocalMemory, text: &str, uri: &str, scope: &str) -> Option<f32> {
        let qv = m.embedder.embed(text);
        let slot = crate::zvec_index::slot_of(uri, scope);
        m.vindex.as_ref().unwrap().search(&qv, 64).unwrap().into_iter().find(|h| h.slot == slot).map(|h| h.score)
    }

    /// The newest `manifest.N` file of an index directory.
    #[cfg(feature = "zvec")]
    fn newest_manifest(vdir: &std::path::Path) -> std::path::PathBuf {
        std::fs::read_dir(vdir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n: u64 = e.file_name().to_string_lossy().strip_prefix("manifest.")?.parse().ok()?;
                Some((n, e.path()))
            })
            .max_by_key(|(n, _)| *n)
            .expect("an index has a manifest")
            .1
    }

    /// A real index with two records, flushed and closed, leaving `m` without a vector index.
    #[cfg(feature = "zvec")]
    fn two_records_flushed(vdir: &std::path::Path) -> (LocalMemory, String, String) {
        let mut m = LocalMemory::for_test_vec(tmp_store(), vdir);
        let a = m.remember("zebra pinned certificate rotation note", "resources/notes", None, None, None).unwrap();
        let b = m.remember("quartz backup restore drill for the archive", "resources/notes", None, None, None).unwrap();
        m.vindex.as_ref().unwrap().flush().unwrap();
        m.vindex = None; // closes the collection (the only handle)
        (m, a, b)
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn a_torn_index_is_quarantined_and_rebuilt_instead_of_crashing_the_open() {
        // The 27-10-2026 failure: a power-off cut a flush off and left the newest manifest at
        // 0 bytes; zvec's open then died with SIGSEGV on every restart. Were the guard missing,
        // this test process would die the same way.
        let vdir = tmp_vdir();
        let (mut m, a, b) = two_records_flushed(&vdir);
        let torn = newest_manifest(&vdir);
        std::fs::write(&torn, b"").unwrap();
        assert!(crate::zvec_index::torn_reason(&vdir).is_some());
        let idx = open_vector_index(&m.store, &vdir, &*m.embedder, false, false).unwrap().expect("a fresh index, not keyword-only");
        let parent = vdir.parent().unwrap();
        let name = vdir.file_name().unwrap().to_string_lossy().into_owned();
        let moved: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("{name}.corrupt-")))
            .collect();
        assert_eq!(moved.len(), 1, "the torn index is kept, next to the new one");
        assert!(moved[0].path().join(torn.file_name().unwrap()).exists());
        assert!(!backfill_marker(&vdir).exists(), "the local open ran the backfill and cleared the marker");
        m.vindex = Some(std::sync::Arc::new(idx));
        for (uri, text) in [(&a, "zebra pinned certificate rotation note"), (&b, "quartz backup restore drill for the archive")] {
            assert!(vec_score(&m, text, uri).is_some_and(|c| c > 0.99), "{uri} has its vector back");
        }
        assert_eq!(m.recall("zebra certificate rotation", 3).unwrap().first().map(|e| e.uri.clone()), Some(a));
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn a_fresh_index_beside_records_is_marked_and_the_deferred_backfill_fills_it() {
        // The other half of 27-10-2026: an index moved aside by hand came back fresh and EMPTY,
        // and 0.3.8 never filled it, so every record dropped out of hybrid recall.
        let vdir = tmp_vdir();
        let (mut m, a, _) = two_records_flushed(&vdir);
        std::fs::rename(&vdir, vdir.with_extension("aside")).unwrap();
        let idx = open_vector_index(&m.store, &vdir, &*m.embedder, false, true).unwrap().unwrap();
        assert!(backfill_marker(&vdir).exists(), "deferred: the server runs it, so the marker stays");
        m.vindex = Some(std::sync::Arc::new(idx));
        assert!(vec_score(&m, "zebra pinned certificate rotation note", &a).is_none(), "nothing backfilled yet");
        let n = backfill_vectors(&m.store, m.vindex.as_ref().unwrap(), &*m.embedder, &vdir).unwrap();
        assert!(n >= 2, "{n}");
        assert!(!backfill_marker(&vdir).exists() && !backfill_progress(&vdir).exists());
        assert!(vec_score(&m, "zebra pinned certificate rotation note", &a).is_some_and(|c| c > 0.99));
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn a_fresh_index_on_an_empty_store_needs_no_backfill() {
        let vdir = tmp_vdir();
        let m = LocalMemory::for_test(tmp_store());
        assert!(open_vector_index(&m.store, &vdir, &*m.embedder, false, true).unwrap().is_some());
        assert!(!backfill_marker(&vdir).exists());
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn scoped_forget_drops_only_its_own_slot_and_the_last_row_drops_every_slot() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let global_text = "storage pool curated operator procedure";
        let room_text = "shadow draft scribbles zebra";
        let uri = save_as(&mut m, "", global_text);
        save_as(&mut m, "room:a", room_text);
        assert!(slot_score(&m, global_text, &uri, "").unwrap() > 0.99, "the room save left the global slot alone");
        assert!(slot_score(&m, room_text, &uri, "room:a").unwrap() > 0.99, "the room record has its own slot");
        as_scope(&mut m, "room:a");
        assert_eq!(m.forget(&uri).unwrap(), 1);
        assert_eq!(slot_score(&m, room_text, &uri, "room:a"), None, "the retracted record's slot is gone");
        assert!(slot_score(&m, global_text, &uri, "").unwrap() > 0.99, "the global record's slot stays");
        assert!(vec_score(&m, room_text, &uri).unwrap() < 0.9, "the retracted room text no longer ranks the uri");
        // the last live row goes: so does the last slot
        as_full(&mut m);
        assert_eq!(m.forget(&uri).unwrap(), 1);
        assert_eq!(vec_score(&m, global_text, &uri), None, "no live row left: no vector of the uri");
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn full_tenant_forget_drops_every_scopes_slot() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let uri = save_as(&mut m, "", "storage pool curated operator procedure");
        save_as(&mut m, "room:a", "shadow draft scribbles zebra");
        as_full(&mut m);
        m.remember("an unrelated global record keeps the index non-empty", "resources/notes", None, None, None).unwrap();
        assert_eq!(m.forget(&uri).unwrap(), 2);
        assert_eq!(vec_score(&m, "shadow draft scribbles zebra", &uri), None);
        assert_eq!(vec_score(&m, "storage pool curated operator procedure", &uri), None);
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn scoped_forget_of_a_room_only_record_drops_its_vector() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let text = "room only note about kestrel mounts";
        let uri = save_as(&mut m, "room:a", text);
        as_full(&mut m);
        m.remember("an unrelated global record keeps the index non-empty", "resources/notes", None, None, None).unwrap();
        assert!(vec_score(&m, text, &uri).is_some());
        as_scope(&mut m, "room:a");
        assert_eq!(m.forget(&uri).unwrap(), 1);
        assert_eq!(vec_score(&m, text, &uri), None);
    }

    // --- E2: the invalidate cascade and dead-uri visibility through the engine ---

    #[test]
    fn room_invalidate_before_start_cascades_and_other_rooms_never_see_it() {
        let mut m = LocalMemory::for_test(tmp_store());
        as_full(&mut m);
        let anchor = m.import_record(Kind::Org, "resources/entities", "Anchor org", "global anchor").unwrap();
        let r = save_as(&mut m, "room:a", "room a detail");
        as_scope(&mut m, "room:a");
        m.link(&r, &anchor, "about").unwrap();
        assert_eq!(m.edges_of(&anchor).unwrap().len(), 1, "room a sees its own edge");
        // valid_to before the record's start: the whole segment goes, nothing live remains
        assert_eq!(m.invalidate(&r, 1_000).unwrap(), 1);
        assert_eq!(m.store.raw_edge_count(&r), 0, "the edge is deleted with the last live row");
        as_scope(&mut m, "room:b");
        assert!(m.edges_of(&anchor).unwrap().is_empty(), "room b's edges(anchor) does not show it");
        assert!(!m.store.scope_visible(&r).unwrap(), "the dead room uri is invisible to room b");
        as_scope(&mut m, "room:a");
        assert!(m.store.scope_visible(&r).unwrap(), "and visible to its own room");
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn invalidate_that_leaves_no_live_row_drops_the_vector() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let text = "room note on the quarterly lantern inventory";
        let uri = save_as(&mut m, "room:a", text);
        as_full(&mut m);
        m.remember("an unrelated global record keeps the index non-empty", "resources/notes", None, None, None).unwrap();
        as_scope(&mut m, "room:a");
        // a cut after the start keeps a live remainder: the vector stays
        assert_eq!(m.invalidate(&uri, now_ms() + 60_000).unwrap(), 1);
        assert!(vec_score(&m, text, &uri).unwrap() > 0.99);
        // a cut before the start removes the last live row: the vector goes too
        assert_eq!(m.invalidate(&uri, 1_000).unwrap(), 1);
        assert_eq!(vec_score(&m, text, &uri), None);
    }

    // --- E4: the newest VISIBLE row wins (uri shadowing) ---

    /// The daimon-hub scenario: a global runbook T; room a saves the same kind/namespace/title
    /// (so the same uri, a newer row in another scope). Returns T's uri.
    fn shadowed_global(m: &mut LocalMemory) -> String {
        as_full(m);
        let g = m.import_record(Kind::Runbook, SHARED_NS, "zzh shared runbook", "operator curated").unwrap();
        // a later millisecond: the room row is the newer one, so "newest visible" is decided
        std::thread::sleep(std::time::Duration::from_millis(3));
        as_scope(m, "room:a");
        let a = m.import_record(Kind::Runbook, SHARED_NS, "zzh shared runbook", "room shadow").unwrap();
        assert_eq!(a, g, "same kind/namespace/title: the same uri");
        g
    }

    #[test]
    fn room_save_does_not_hide_a_global_record_elsewhere() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = shadowed_global(&mut m);
        as_scope(&mut m, "room:b");
        let hits = m.recall("zzh shared runbook", 10).unwrap();
        assert!(hits.iter().any(|h| h.uri == g && h.scope.is_empty() && h.body == "operator curated"), "{hits:?}");
        assert!(hits.iter().all(|h| h.scope != "room:a"), "room a's record never reaches room b");
        // room a sees both records under the uri; recall gives it ONE hit for the uri, its own
        // copy (the reader's own write scope first), whichever text the query favours
        as_scope(&mut m, "room:a");
        for q in ["zzh shared runbook", "zzh shared runbook operator curated", "zzh shared runbook room shadow"] {
            let hits = m.recall(q, 10).unwrap();
            let under: Vec<&str> = hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect();
            assert_eq!(under, vec!["room:a"], "{q}: {hits:?}");
        }
        // a reader that only reads room a (no write scope) gets the newest: room a's copy too
        m.set_scope_context(None, Some(vec!["room:a".into()]));
        let hits = m.recall("zzh shared runbook operator curated", 10).unwrap();
        assert_eq!(hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec!["room:a"]);
        // the full-tenant reader (write scope '') gets the global record
        as_full(&mut m);
        let hits = m.recall("zzh shared runbook room shadow", 10).unwrap();
        assert_eq!(hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec![""]);
    }

    #[test]
    fn room_save_does_not_hide_a_global_record_from_graph_riders() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = shadowed_global(&mut m);
        as_full(&mut m);
        let anchor = m.import_record(Kind::Org, "resources/entities", "Quokka anchor", "quokka anchor org").unwrap();
        m.link(&anchor, &g, "about").unwrap();
        as_scope(&mut m, "room:b");
        let graph = m.recall_expanded_graph("quokka anchor", 3, 1).unwrap();
        assert!(graph.seeds.iter().any(|e| e.uri == anchor));
        let rider = graph.riders.iter().find(|r| r.entry.uri == g).expect("the global record rides for room b");
        assert_eq!((rider.entry.scope.as_str(), rider.entry.body.as_str()), ("", "operator curated"));
        assert!(m.edges_of(&anchor).unwrap().iter().any(|e| e.to_uri == g), "and its edge is on room b's graph");
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn room_save_keeps_the_global_vector_and_hybrid_recall_finds_it() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let g = shadowed_global(&mut m);
        assert!(slot_score(&m, "operator curated", &g, "").unwrap() > 0.99, "the global slot still holds global text");
        assert!(slot_score(&m, "room shadow", &g, "room:a").unwrap() > 0.99, "room a's text lives in room a's slot");
        as_scope(&mut m, "room:b");
        let hits = m.recall("zzh shared runbook operator curated", 10).unwrap();
        assert!(hits.iter().any(|h| h.uri == g && h.scope.is_empty() && h.body == "operator curated"), "{hits:?}");
        assert!(hits.iter().all(|h| h.scope != "room:a"));
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn vector_hits_resolve_to_their_own_record_and_invisible_slots_drop() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let g = shadowed_global(&mut m);
        let qv = m.embedder.embed("room shadow");
        let search = |m: &LocalMemory| m.vindex.as_ref().unwrap().search(&qv, 16).unwrap();
        // room b: the room slot (best match, cosine 1.0) is not room b's to see -> dropped; the
        // global slot resolves to the GLOBAL record, never to room a's row under the same uri
        as_scope(&mut m, "room:b");
        let mut entries = std::collections::HashMap::new();
        let vec = m.resolve_vector_hits(search(&m), &mut entries).unwrap();
        assert!(vec.iter().all(|(k, _)| k != &record_key(&g, "room:a")), "room a's slot never ranks for room b");
        let (k, score) = vec.iter().find(|(k, _)| k == &record_key(&g, "")).expect("the global slot resolves");
        assert!(*score < 0.9, "ranked by its OWN text, not the room's: {score}");
        assert_eq!(entries[k].body, "operator curated");
        // room a: its own slot resolves to its own record
        as_scope(&mut m, "room:a");
        let mut entries = std::collections::HashMap::new();
        let vec = m.resolve_vector_hits(search(&m), &mut entries).unwrap();
        let (k, score) = &vec[0];
        assert_eq!(k, &record_key(&g, "room:a"));
        assert!(*score > 0.99);
        assert_eq!(entries[k].body, "room shadow");
        // a stale slot (its record retracted underneath the index) resolves to nothing
        m.store.forget_in(&g, Some("room:a")).unwrap();
        let mut entries = std::collections::HashMap::new();
        let vec = m.resolve_vector_hits(search(&m), &mut entries).unwrap();
        assert!(vec.iter().all(|(k, _)| k != &record_key(&g, "room:a")));
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn opens_a_pre_slot_store_and_migrates_its_vectors() {
        // A 0.3.6-format data dir: the SQLite store (its schema is unchanged in 0.3.7) and a
        // vector index with ONE vector per uri under the bare-uri key - whichever scope saved
        // last. Here room a saved after the global record, so the global record's key holds
        // the room's text, and the room record has no slot of its own.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmtools-mig-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, vdir) = (dir.join("t.db"), dir.join("vectors").join("t"));
        let embed = crate::embedder::HashEmbedder::new();
        let (g_text, r_text, only_text) = ("operator curated steps", "room shadow draft", "a global only record");
        let (g, only) = {
            use crate::embedder::Embedder;
            let s = SqliteStore::open(&db).unwrap();
            let uri = make_uri(SHARED_NS, Kind::Runbook, "zzh shared runbook");
            let mut e = Entry::new_now(uri.clone(), Kind::Runbook, SHARED_NS.into(), "zzh shared runbook".into(),
                g_text.into(), vec![], 60, uri.clone());
            s.put(&e).unwrap();
            e.scope = "room:a".into();
            e.body = r_text.into();
            s.put(&e).unwrap();
            let o = make_uri(SHARED_NS, Kind::Runbook, "only global");
            s.put(&Entry::new_now(o.clone(), Kind::Runbook, SHARED_NS.into(), "only global".into(),
                only_text.into(), vec![], 60, o.clone())).unwrap();
            // the legacy writes: bare-uri key = today's global slot
            let idx = crate::zvec_index::ZvecIndex::open(&vdir).unwrap();
            idx.upsert_chunks(&uri, "", &embed.embed_chunks(r_text)).unwrap();
            idx.upsert_chunks(&o, "", &embed.embed_chunks(only_text)).unwrap();
            (uri, o)
        };
        assert!(!slot_marker(&vdir).exists(), "a pre-slot index carries no marker");

        // start-up on the old data dir runs the migration
        let mut m = LocalMemory::for_test_vec(SqliteStore::open(&db).unwrap(), &vdir);
        assert!(slot_marker(&vdir).exists(), "the migration is recorded");
        assert!(slot_score(&m, g_text, &g, "").unwrap() > 0.99, "the global slot is rebuilt from the global row");
        assert!(slot_score(&m, r_text, &g, "").unwrap() < 0.9, "and no longer holds the room's text");
        assert!(slot_score(&m, r_text, &g, "room:a").unwrap() > 0.99, "the room record got its own slot");
        assert!(slot_score(&m, only_text, &only, "").unwrap() > 0.99, "a global-only uri is untouched");
        as_scope(&mut m, "room:b");
        let hits = m.recall("zzh shared runbook operator curated steps", 10).unwrap();
        assert!(hits.iter().any(|h| h.uri == g && h.scope.is_empty()), "room b finds the global record: {hits:?}");
        drop(m);

        // a uri held ONLY by a room loses its stale bare-uri (global) slot; a global record whose
        // room row is already closed gets its own text back in its slot
        let dir2 = dir.join("second");
        let (db2, vdir2) = (dir2.join("t.db"), dir2.join("vectors").join("t"));
        std::fs::create_dir_all(&dir2).unwrap();
        let (room_only, healed) = {
            use crate::embedder::Embedder;
            let s = SqliteStore::open(&db2).unwrap();
            let uri = make_uri(SHARED_NS, Kind::Runbook, "room only");
            let mut e = Entry::new_now(uri.clone(), Kind::Runbook, SHARED_NS.into(), "room only".into(),
                r_text.into(), vec![], 60, uri.clone());
            e.scope = "room:a".into();
            s.put(&e).unwrap();
            let h = make_uri(SHARED_NS, Kind::Runbook, "was shadowed");
            let mut he = Entry::new_now(h.clone(), Kind::Runbook, SHARED_NS.into(), "was shadowed".into(),
                g_text.into(), vec![], 60, h.clone());
            s.put(&he).unwrap();
            he.scope = "room:a".into();
            he.body = r_text.into();
            s.put(&he).unwrap();
            s.forget_in(&h, Some("room:a")).unwrap(); // the room row is closed; its text stayed in the vector
            let idx = crate::zvec_index::ZvecIndex::open(&vdir2).unwrap();
            idx.upsert_chunks(&uri, "", &embed.embed_chunks(r_text)).unwrap();
            idx.upsert_chunks(&h, "", &embed.embed_chunks(r_text)).unwrap();
            idx.upsert_chunks("daimon://keeps/the/index/non-empty", "", &embed.embed_chunks(only_text)).unwrap();
            (uri, h)
        };
        let m = LocalMemory::for_test_vec(SqliteStore::open(&db2).unwrap(), &vdir2);
        assert_eq!(slot_score(&m, r_text, &room_only, ""), None, "the room text no longer sits in a global slot");
        assert!(slot_score(&m, r_text, &room_only, "room:a").unwrap() > 0.99);
        assert!(slot_score(&m, g_text, &healed, "").unwrap() > 0.99, "the global slot holds the global text again");
        assert_eq!(slot_score(&m, r_text, &healed, "room:a"), None, "no slot for the closed room row");
    }

    // --- E5: edge ownership in the engine ---

    /// A distinct record titled `title`, saved as `scope` ("" = the full-tenant writer).
    fn rec_as(m: &mut LocalMemory, scope: &str, title: &str) -> String {
        if scope.is_empty() {
            as_full(m);
        } else {
            as_scope(m, scope);
        }
        m.import_record(Kind::Runbook, "resources/workspace", title, &format!("{title} body")).unwrap()
    }

    /// The message of a typed engine refusal (panics on success or on an untyped error).
    fn refusal_of<T: std::fmt::Debug>(r: Result<T>) -> String {
        let e = r.expect_err("expected a refusal");
        e.downcast_ref::<Refusal>().unwrap_or_else(|| panic!("not a typed refusal: {e:#}")).message().to_string()
    }

    #[test]
    fn valid_rel_is_one_lowercase_token() {
        let longest = "a".repeat(32);
        for ok in ["links", "part-of", "made_by", "alias-of", "a", "x1", longest.as_str()] {
            assert!(valid_rel(ok), "{ok:?} is a valid rel");
        }
        let too_long = "a".repeat(33);
        for bad in ["", "Links", "1st", "-x", "_x", "part of", "about!", "caf\u{e9}", "a\n", too_long.as_str()] {
            assert!(!valid_rel(bad), "{bad:?} is not a valid rel");
        }
    }

    #[test]
    fn scoped_link_between_two_global_records_is_refused() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g1 = rec_as(&mut m, "", "Global one");
        let g2 = rec_as(&mut m, "", "Global two");
        as_scope(&mut m, "room:a");
        let msg = refusal_of(m.link(&g1, &g2, "about"));
        assert!(msg.contains("only this identity's write scope holds"), "{msg}");
        assert_eq!(m.store.raw_edge_count(&g1), 0, "nothing was linked");
        // nor may it cut the operator's edge between them
        as_full(&mut m);
        m.link(&g1, &g2, "about").unwrap();
        as_scope(&mut m, "room:a");
        refusal_of(m.unlink(&g1, &g2, "about"));
        assert_eq!(m.store.raw_edge_count(&g1), 1, "the operator's edge stays");
    }

    #[test]
    fn scoped_link_from_an_own_record_to_a_global_record_is_allowed() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = rec_as(&mut m, "", "Global anchor");
        let own = rec_as(&mut m, "room:a", "Room a notes");
        as_scope(&mut m, "room:a");
        m.link(&own, &g, "about").unwrap();
        m.link(&g, &own, "informs").unwrap(); // either direction: one endpoint is wholly owned
        assert_eq!(m.edges_of(&own).unwrap().len(), 2);
        assert_eq!(m.unlink(&own, &g, "about").unwrap(), 1, "and it may remove its own edge");
        assert_eq!(m.edges_of(&own).unwrap().len(), 1);
    }

    #[test]
    fn scoped_link_from_a_uri_another_scope_also_holds_is_refused() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = rec_as(&mut m, "", "Global anchor");
        // room a and room b both hold the shared runbook uri: it is not wholly room a's
        let shared = save_as(&mut m, "room:a", "room a notes");
        save_as(&mut m, "room:b", "room b notes");
        as_scope(&mut m, "room:a");
        refusal_of(m.link(&shared, &g, "about"));
        assert_eq!(m.store.raw_edge_count(&g), 0);
        // a shared uri may still link to a record room a wholly owns
        let own = rec_as(&mut m, "room:a", "Room a only");
        m.link(&shared, &own, "about").unwrap();
        // once room b retracts its row, room a holds the uri outright
        as_scope(&mut m, "room:b");
        m.forget(&shared).unwrap();
        as_scope(&mut m, "room:a");
        m.link(&shared, &g, "about").unwrap();

        // the same when the other holder is global
        let mut m = LocalMemory::for_test(tmp_store());
        let g = rec_as(&mut m, "", "Global anchor");
        let shared = save_as(&mut m, "", "global notes");
        save_as(&mut m, "room:a", "room a notes");
        as_scope(&mut m, "room:a");
        refusal_of(m.link(&shared, &g, "about"));
        refusal_of(m.link(&g, &shared, "about"));
        assert_eq!(m.store.raw_edge_count(&g), 0);
    }

    #[test]
    fn free_text_rel_is_refused_on_link_and_accepted_on_unlink() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = rec_as(&mut m, "", "Global anchor");
        let own = rec_as(&mut m, "room:a", "Room a notes");
        as_scope(&mut m, "room:a");
        let too_long = "x".repeat(33);
        for bad in ["relates to", "Relates", "about!", "", "1st", too_long.as_str()] {
            let msg = refusal_of(m.link(&own, &g, bad));
            assert!(msg.starts_with("invalid rel"), "{bad:?}: {msg}");
        }
        assert_eq!(m.store.raw_edge_count(&own), 0);
        // a legacy free-text edge (made before the rule) can still be removed by its owner
        m.store.link(&own, &g, "relates to the plan").unwrap();
        assert_eq!(m.unlink(&own, &g, "relates to the plan").unwrap(), 1);
        // the full-tenant writer is unchanged: any label, as before
        as_full(&mut m);
        m.link(&own, &g, "Relates To").unwrap();
        m.link(&g, &own, "free text label").unwrap();
        assert_eq!(m.store.raw_edge_count(&own), 2);
    }

    #[test]
    fn edge_guard_hides_invisible_endpoints_and_refuses_write_denied_identities() {
        let mut m = LocalMemory::for_test(tmp_store());
        let own = rec_as(&mut m, "room:a", "Room a notes");
        let other = rec_as(&mut m, "room:b", "Room b notes");
        let g = rec_as(&mut m, "", "Global anchor");
        as_scope(&mut m, "room:a");
        // an endpoint the writer cannot see reads as absent, never as "not yours"
        let msg = refusal_of(m.link(&own, &other, "about"));
        assert!(msg.contains("not found (or not visible to this identity)"), "{msg}");
        // a write-denied identity cannot touch edges, whatever it could read
        m.set_scope_context(None, Some(vec!["room:a".into()]));
        assert!(refusal_of(m.link(&own, &g, "about")).starts_with("this identity has no write scope"));
        assert!(refusal_of(m.link(&own, &g, "Not A Token")).starts_with("this identity has no write scope"), "that reason first");
        assert!(refusal_of(m.unlink(&own, &g, "about")).starts_with("this identity has no write scope"));
        assert_eq!(m.store.raw_edge_count(&own), 0);
        // an agent-labelled full-tenant writer keeps the visibility-only rule (no ownership)
        as_full(&mut m);
        m.set_agent_view(Some("assistant".into()));
        m.link(&other, &g, "about").unwrap();
        assert_eq!(m.store.raw_edge_count(&g), 1);
    }

    // --- E6: explicit-write-scope tenant policy (engine side) ---

    /// Bind `m` as a full-tenant token on a policy tenant whose request named no write scope.
    fn as_policy_unscoped(m: &mut LocalMemory) {
        m.set_scope_context(None, None);
        m.set_explicit_write_scope_missing(true);
    }

    #[test]
    fn missing_explicit_write_scope_refuses_every_write_and_no_read() {
        let mut m = LocalMemory::for_test(tmp_store());
        let g = rec_as(&mut m, "", "Global anchor");
        let other = rec_as(&mut m, "", "Global other");
        as_policy_unscoped(&mut m);
        let writes: Vec<(&str, String)> = vec![
            ("remember", refusal_of(m.remember("a fact", "resources/notes", None, None, None))),
            ("log_decision", refusal_of(m.log_decision("t", "c", "d", "r", "resources/notes", None))),
            ("import_record", refusal_of(m.import_record(Kind::Runbook, "resources/workspace", "New", "b"))),
            ("persona import", refusal_of(m.import_record(Kind::Persona, "agents/assistant", "P", "b"))),
            ("forget", refusal_of(m.forget(&g))),
            ("invalidate", refusal_of(m.invalidate(&g, now_ms() + 60_000))),
            ("link", refusal_of(m.link(&g, &other, "about"))),
            ("unlink", refusal_of(m.unlink(&g, &other, "about"))),
        ];
        for (op, msg) in writes {
            assert_eq!(msg, EXPLICIT_WRITE_SCOPE_REQUIRED, "{op}");
        }
        assert_eq!(m.store.live_scopes(&g), vec![""], "nothing was retracted");
        // reads are unchanged: the full tenant, as before
        assert!(m.recent(10).unwrap().iter().any(|e| e.uri == g));
        assert!(m.history(&g, 5).unwrap().len() == 1);
        // naming a scope (the header) restores writes: '' global, anything else that scope
        m.set_explicit_write_scope_missing(false);
        m.set_scope_context(Some(""), None);
        let n = m.remember("explicitly global", "resources/notes", None, None, None).unwrap();
        m.set_scope_context(Some("room:x"), None);
        let r = m.remember("explicitly room x", "resources/notes", None, None, None).unwrap();
        assert_eq!(m.store.live_scopes(&n), vec![""]);
        assert_eq!(m.store.live_scopes(&r), vec!["room:x"]);
    }

    #[test]
    fn write_denied_and_guard_refusals_are_typed() {
        let mut m = LocalMemory::for_test(tmp_store());
        let own = rec_as(&mut m, "room:a", "Room a notes");
        let g = rec_as(&mut m, "", "Global anchor");
        // a read-only identity: the old reason, now a typed refusal
        m.set_scope_context(None, Some(vec!["room:a".into()]));
        let msg = refusal_of(m.remember("x", "resources/notes", None, None, None));
        assert!(msg.starts_with("this identity has no write scope"), "{msg}");
        assert!(refusal_of(m.import_record(Kind::Persona, "agents/assistant", "P", "b")).starts_with("this identity has no write scope"));
        // the retraction guard's refusals (the server used to answer /forget with 500)
        as_scope(&mut m, "room:b");
        assert_eq!(refusal_of(m.forget(&own)), "record not found in this identity's write scope");
        assert_eq!(refusal_of(m.forget(&g)), "record not found in this identity's write scope");
        // the agents/ tree guard
        assert_eq!(refusal_of(m.import_record(Kind::Persona, "agents/assistant", "P", "b")), "scoped identities cannot write into the agents/ tree");
        as_full(&mut m);
        m.set_agent_view(Some("analyst".into()));
        assert!(refusal_of(m.import_record(Kind::Persona, "agents/assistant", "P", "b")).contains("belongs to another agent identity"));
    }

    #[test]
    fn embedded_write_scope_follows_the_flag_unless_dm_scope_binds_the_session() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(embedded_write_scope(None, None).unwrap(), None, "no flag: the session's own");
        assert_eq!(embedded_write_scope(None, s("room:x")).unwrap(), s("room:x"));
        assert_eq!(embedded_write_scope(None, s("")).unwrap(), s(""), "'' = a deliberate global write");
        assert_eq!(embedded_write_scope(s("user:a"), None).unwrap(), None);
        assert_eq!(embedded_write_scope(s("user:a"), s("user:a")).unwrap(), None, "the bound scope again: no-op");
        assert!(embedded_write_scope(s("user:a"), s("room:x")).is_err(), "another scope is refused, not ignored");
        assert!(embedded_write_scope(s("user:a"), s("")).is_err(), "a DM_SCOPE session cannot promote to global");
    }

    // --- E7: import_record_at goes through the scope and namespace guards ---

    /// Every row (live or closed) stored under `uri`, as (scope, importance, created_ms).
    fn all_rows(m: &LocalMemory, uri: &str) -> Vec<(String, i64, i64)> {
        m.store.raw_rows(uri)
    }

    #[test]
    fn restricted_import_at_is_stamped_and_keeps_the_migration_levers_for_the_operator() {
        let mut m = LocalMemory::for_test(tmp_store());
        as_scope(&mut m, "room:a");
        // before E7 this path ignored the write scope and landed GLOBAL
        let uri = m.import_record_at(Kind::Memory, "resources/notes", "Room a import", "b", 0, None).unwrap();
        let rows = all_rows(&m, &uri);
        assert_eq!((rows.len(), rows[0].0.as_str(), rows[0].1), (1, "room:a", 60), "stamped with the write scope");
        // importance at or below the kind default is fine (it can only sink the record)
        let low = m.import_record_at(Kind::Memory, "resources/notes", "Room a low", "b", 0, Some(50)).unwrap();
        assert_eq!(all_rows(&m, &low)[0].0, "room:a");
        assert_eq!(all_rows(&m, &low)[0].1, 50);
        m.import_record_at(Kind::Memory, "resources/notes", "Room a default", "b", 0, Some(60)).unwrap();
        // above the kind default: refused, nothing stored
        for (kind, imp) in [(Kind::Memory, 61), (Kind::Chunk, 50), (Kind::Protocol, 99)] {
            let msg = refusal_of(m.import_record_at(kind, "resources/notes", "Raised", "b", 0, Some(imp)));
            assert_eq!(msg, IMPORTANCE_IS_SET_BY_THE_OPERATOR, "{kind:?} {imp}");
            assert!(all_rows(&m, &make_uri("resources/notes", kind, "Raised")).is_empty());
        }
        // backdating: refused, nothing stored
        let msg = refusal_of(m.import_record_at(Kind::Memory, "resources/notes", "Backdated", "b", 1_600_000_000_000, None));
        assert_eq!(msg, CREATED_MS_IS_SET_BY_THE_OPERATOR);
        assert!(all_rows(&m, &make_uri("resources/notes", Kind::Memory, "Backdated")).is_empty());
        // a plain scoped token is restricted the same way
        m.set_scope_context(Some("user:alice"), Some(vec!["user:alice".into()]));
        refusal_of(m.import_record_at(Kind::Memory, "resources/notes", "Alice backdated", "b", 1_600_000_000_000, None));
    }

    #[test]
    fn import_at_refuses_write_denied_identities_and_foreign_agent_trees() {
        let mut m = LocalMemory::for_test(tmp_store());
        // a read-only identity (an adapter request without X-DM-Write-Scope): nothing saved
        m.set_scope_context(None, Some(vec!["room:a".into()]));
        let msg = refusal_of(m.import_record_at(Kind::Memory, "resources/notes", "No scope", "b", 1_600_000_000_000, None));
        assert!(msg.starts_with("this identity has no write scope"), "{msg}");
        assert!(all_rows(&m, &make_uri("resources/notes", Kind::Memory, "No scope")).is_empty());
        // a scoped identity cannot plant a (backdated) record in an agent's persona tree
        as_scope(&mut m, "room:a");
        let msg = refusal_of(m.import_record_at(Kind::Persona, "agents/analyst", "Planted", "b", 1_600_000_000_000, None));
        assert_eq!(msg, "scoped identities cannot write into the agents/ tree");
        // an agent-labelled full-tenant writer cannot reach another agent's tree either
        as_full(&mut m);
        m.set_agent_view(Some("assistant".into()));
        let msg = refusal_of(m.import_record_at(Kind::Persona, "agents/analyst", "Planted", "b", 1_600_000_000_000, Some(95)));
        assert!(msg.contains("belongs to another agent identity"), "{msg}");
        assert!(all_rows(&m, &make_uri("agents/analyst", Kind::Persona, "Planted")).is_empty());
        // a tenant that requires an explicit write scope refuses the unscoped import
        m.set_agent_view(None);
        as_policy_unscoped(&mut m);
        assert_eq!(refusal_of(m.import_record_at(Kind::Memory, "resources/notes", "Policy", "b", 1_600_000_000_000, Some(80))), EXPLICIT_WRITE_SCOPE_REQUIRED);
    }

    #[test]
    fn full_tenant_import_at_is_unchanged() {
        let mut m = LocalMemory::for_test(tmp_store());
        let t0 = 1_600_000_000_000;
        let uri = m.import_record_at(Kind::Protocol, "agent/protocol", "Migrated rule", "b", t0, Some(99)).unwrap();
        assert_eq!(all_rows(&m, &uri), vec![(String::new(), 99, t0)], "global, importance and created_ms kept");
        let e = m.store.get(&uri).unwrap().unwrap();
        assert_eq!(e.valid_from_ms, t0, "valid time starts at the original creation");
        // with the explicit-write-scope policy satisfied ('' named), the same
        m.set_explicit_write_scope_missing(false);
        m.set_scope_context(Some(""), None);
        let uri2 = m.import_record_at(Kind::Memory, "resources/notes", "Migrated note", "b", t0, Some(80)).unwrap();
        assert_eq!(all_rows(&m, &uri2), vec![(String::new(), 80, t0)]);
    }

    // --- review follow-up: a read-restricted GLOBAL writer is not a full-tenant writer ---

    /// Bind `m` as an identity that writes global ('') but reads only `reads` + global: an
    /// adapter promoting with `X-DM-Write-Scope: ''`, or a scoped token minted with a ''
    /// write scope.
    fn as_restricted_global_writer(m: &mut LocalMemory, reads: &[&str]) {
        m.set_scope_context(Some(""), Some(reads.iter().map(|s| s.to_string()).collect()));
    }

    #[test]
    fn read_restricted_global_writer_is_bounded_to_global_rows() {
        let mut m = LocalMemory::for_test(tmp_store());
        // a uri held only by room b and user bob, with an edge; two room b records with an edge
        let hidden = save_as(&mut m, "room:b", "room b salary notes");
        save_as(&mut m, "user:bob", "bob salary notes");
        let anchor = link_anchor(&mut m, &hidden);
        let b1 = rec_as(&mut m, "room:b", "Room b one");
        let b2 = rec_as(&mut m, "room:b", "Room b two");
        as_full(&mut m);
        m.link(&b1, &b2, "about").unwrap();

        as_restricted_global_writer(&mut m, &["room:a"]);
        assert!(m.history(&hidden, 10).unwrap().is_empty(), "it cannot even read the uri");
        // forget / invalidate of rows it cannot see: refused, nothing closed, no count oracle
        assert_eq!(refusal_of(m.forget(&hidden)), "record not found in this identity's write scope");
        assert_eq!(refusal_of(m.invalidate(&hidden, now_ms() + 60_000)), "record not found in this identity's write scope");
        assert_eq!(m.store.live_scopes(&hidden), vec!["room:b", "user:bob"]);
        assert_eq!(m.store.raw_edge_count(&hidden), 1);
        // edges between records it cannot see: refused as not found
        assert!(refusal_of(m.unlink(&b1, &b2, "about")).contains("not found (or not visible to this identity)"));
        assert!(refusal_of(m.link(&b1, &b2, "about")).contains("not found (or not visible to this identity)"));
        assert!(refusal_of(m.link(&hidden, &anchor, "about")).contains("not found (or not visible to this identity)"));
        assert_eq!(m.store.raw_edge_count(&b1), 1);
        // the migration levers and the agents/ tree stay the operator's
        assert_eq!(refusal_of(m.import_record_at(Kind::Protocol, "agent/protocol", "Planted rule", "b", 0, Some(99))), IMPORTANCE_IS_SET_BY_THE_OPERATOR);
        assert_eq!(refusal_of(m.import_record_at(Kind::Memory, "resources/notes", "Backdated", "b", 1_600_000_000_000, None)), CREATED_MS_IS_SET_BY_THE_OPERATOR);
        assert_eq!(refusal_of(m.import_record(Kind::Persona, "agents/analyst", "Analyst persona", "b")), "scoped identities cannot write into the agents/ tree");
        assert_eq!(refusal_of(m.import_record_at(Kind::Persona, "agents/analyst", "Analyst persona", "b", 0, None)), "scoped identities cannot write into the agents/ tree");
        assert!(all_rows(&m, &make_uri("agents/analyst", Kind::Persona, "Analyst persona")).is_empty());
        assert!(all_rows(&m, &make_uri("agent/protocol", Kind::Protocol, "Planted rule")).is_empty());
        // a free-text rel is refused (the label is readable by every audience of the endpoints)
        let g1 = { as_full(&mut m); m.import_record(Kind::Org, "resources/entities", "Global one", "g1").unwrap() };
        let g2 = m.import_record(Kind::Org, "resources/entities", "Global two", "g2").unwrap();
        as_restricted_global_writer(&mut m, &["room:a"]);
        assert!(refusal_of(m.link(&g1, &g2, "Room A was here")).starts_with("invalid rel"));

        // what it IS: a global writer. It saves global, and retracts or links GLOBAL rows only
        let promoted = m.remember("promoted to everyone", "resources/workspace", None, None, None).unwrap();
        assert_eq!(m.store.live_scopes(&promoted), vec![""]);
        let low = m.import_record_at(Kind::Memory, "resources/notes", "Low", "b", 0, Some(40)).unwrap();
        assert_eq!(all_rows(&m, &low).iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), vec![("", 40)]);
        m.link(&g1, &g2, "about").unwrap();
        assert_eq!(m.unlink(&g1, &g2, "about").unwrap(), 1);
        // a uri held by global AND room b: it closes the global row only; the node stays
        let shared = rec_as(&mut m, "", "Shared two");
        assert_eq!(rec_as(&mut m, "room:b", "Shared two"), shared);
        link_anchor(&mut m, &shared);
        as_restricted_global_writer(&mut m, &["room:a"]);
        assert_eq!(m.forget(&shared).unwrap(), 1, "the global row only, and only it is counted");
        assert_eq!(m.store.live_scopes(&shared), vec!["room:b"]);
        assert_eq!(m.store.raw_edge_count(&shared), 1, "room b still holds the node: edges stay");
        // E5 with ws = '': an edge needs one endpoint held by global alone. The shared uri
        // (global + room b) and a room a record are not; a global-only record is.
        as_full(&mut m);
        m.import_record(Kind::Runbook, "resources/workspace", "Shared two", "global again").unwrap();
        let ra = rec_as(&mut m, "room:a", "Room a only");
        as_restricted_global_writer(&mut m, &["room:a"]);
        assert!(refusal_of(m.link(&shared, &ra, "about")).contains("only this identity's write scope holds"));
        m.link(&ra, &anchor, "about").unwrap();
        m.link(&shared, &anchor, "about").unwrap();

        // the full-tenant writer (reads the whole tenant) keeps every lever, as before
        as_full(&mut m);
        assert_eq!(m.forget(&hidden).unwrap(), 2);
        m.import_record_at(Kind::Protocol, "agent/protocol", "Operator rule", "b", 1_600_000_000_000, Some(99)).unwrap();
    }

    #[test]
    fn batch_graph_maintenance_is_full_tenant_only() {
        let mut m = LocalMemory::for_test(tmp_store());
        let target = m.remember("Kestrel hub notes", "resources/notes", None, None, None).unwrap();
        m.set_scope_context(Some("room:b"), None);
        m.remember("room b links [[Kestrel hub notes]]", "resources/notes", None, None, None).unwrap();
        let before = m.store.raw_edge_count(&target);
        let msg = "batch graph maintenance needs a full-tenant token writing global";
        // a room writer (a full-tenant token that named a room), a scoped identity, and a
        // read-restricted global writer: refused, nothing written or pruned
        for (w, r) in [(Some("room:b"), None), (Some("room:a"), Some(vec!["room:a".to_string()])), (Some(""), Some(vec!["room:a".to_string()]))] {
            m.set_scope_context(w, r.clone());
            assert_eq!(refusal_of(m.reindex_links()), msg, "{w:?} {r:?}");
            assert_eq!(refusal_of(m.reindex_mentions(false)), msg);
            assert_eq!(refusal_of(m.reindex_mentions(true)), msg);
        }
        // write-denied identities hear their own reason
        m.set_scope_context(None, Some(vec!["room:a".into()]));
        assert!(refusal_of(m.reindex_links()).starts_with("this identity has no write scope"));
        as_policy_unscoped(&mut m);
        assert_eq!(refusal_of(m.reindex_links()), EXPLICIT_WRITE_SCOPE_REQUIRED);
        assert_eq!(refusal_of(m.reindex_mentions(true)), EXPLICIT_WRITE_SCOPE_REQUIRED);
        assert_eq!(m.store.raw_edge_count(&target), before);
        // the full-tenant writer runs it
        m.set_explicit_write_scope_missing(false);
        as_full(&mut m);
        assert_eq!(m.reindex_links().unwrap().0, 1);
        assert_eq!(m.store.raw_edge_count(&target), before + 1);
    }

    /// Recall signals are per record: a room recalling its own copy of a shared title bumps
    /// that copy, never the global record under the same uri that other audiences rank by. A
    /// signal row stored before 0.3.7 (keyed by the bare uri) stays the global record's.
    fn signals_follow_the_recalled_record(mut m: LocalMemory) {
        let uri = save_as(&mut m, "", "kestrel signal notes: global procedure");
        save_as(&mut m, "room:a", "kestrel signal notes: room a draft");
        m.store.bump_signal(&uri, 1_000).unwrap(); // the 0.3.6 form: bare uri
        let count = |m: &LocalMemory, scope: &str| {
            let k = signal_key(&uri, scope);
            m.store.read_signals(std::slice::from_ref(&k)).unwrap().get(&k).map(|s| s.0).unwrap_or(0)
        };
        as_scope(&mut m, "room:a");
        for _ in 0..3 {
            let hits = m.recall("kestrel signal notes", 5).unwrap();
            assert_eq!(hits.iter().filter(|h| h.uri == uri).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec!["room:a"]);
        }
        assert_eq!((count(&m, ""), count(&m, "room:a")), (1, 3), "room a's recalls leave the global record's signal alone");
        as_scope(&mut m, "room:b");
        let hits = m.recall("kestrel signal notes", 5).unwrap();
        assert_eq!(hits.iter().filter(|h| h.uri == uri).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec![""]);
        assert_eq!((count(&m, ""), count(&m, "room:a"), count(&m, "room:b")), (2, 3, 0), "room b recalled the global record");
    }

    #[test]
    fn recall_signals_are_per_record_keyword() {
        signals_follow_the_recalled_record(LocalMemory::for_test(tmp_store()));
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn recall_signals_are_per_record_hybrid() {
        signals_follow_the_recalled_record(LocalMemory::for_test_vec(tmp_store(), &tmp_vdir()));
    }

    #[test]
    fn recall_gives_one_hit_per_uri_to_a_reader_that_sees_several_copies() {
        // the full-tenant reader sees the global record and two rooms' copies of one title:
        // one hit for the uri (its own write scope's copy, the global one), the rest of the
        // limit goes to other records
        let mut m = LocalMemory::for_test(tmp_store());
        // the three copies match the query best, so without the cut they would take 3 of 4 places
        let copy = "zeta rollout checklist: zeta rollout checklist, zeta rollout checklist";
        let g = save_as(&mut m, "", &format!("{copy} global"));
        save_as(&mut m, "room:a", &format!("{copy} room a"));
        save_as(&mut m, "room:b", &format!("{copy} room b"));
        as_full(&mut m);
        for i in 0..3 {
            m.remember(&format!("zeta rollout checklist note {i}"), "resources/notes", None, None, None).unwrap();
        }
        let hits = m.recall("zeta rollout checklist", 4).unwrap();
        let uris: std::collections::HashSet<&str> = hits.iter().map(|h| h.uri.as_str()).collect();
        assert_eq!((hits.len(), uris.len()), (4, 4), "4 distinct uris: {hits:?}");
        assert_eq!(hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec![""]);
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn hybrid_recall_gives_one_hit_per_uri() {
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let copy = "zeta rollout checklist: zeta rollout checklist, zeta rollout checklist";
        let g = save_as(&mut m, "", &format!("{copy} global"));
        save_as(&mut m, "room:a", &format!("{copy} room a"));
        save_as(&mut m, "room:b", &format!("{copy} room b"));
        as_full(&mut m);
        for i in 0..3 {
            m.remember(&format!("zeta rollout note {i}"), "resources/notes", None, None, None).unwrap();
        }
        let hits = m.recall("zeta rollout checklist", 4).unwrap();
        let uris: std::collections::HashSet<&str> = hits.iter().map(|h| h.uri.as_str()).collect();
        assert_eq!((hits.len(), uris.len()), (4, 4), "4 distinct uris: {hits:?}");
        assert_eq!(hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec![""]);
        // a room reader sees the global record and its own copy: one hit, its own
        as_scope(&mut m, "room:a");
        let hits = m.recall("zeta rollout checklist global", 10).unwrap();
        assert_eq!(hits.iter().filter(|h| h.uri == g).map(|h| h.scope.as_str()).collect::<Vec<_>>(), vec!["room:a"]);
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn invisible_slots_do_not_crowd_a_gated_readers_vector_channel() {
        // Two global records match the query exactly, a third (G) less closely; 40 room b
        // records sit between them in cosine. For a room c reader the nearest `pool` slots
        // were nearly all room b's, G never reached the vector channel, and in hybrid mode
        // (cosine is the floor) the keyword channel could not bring it back.
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        as_full(&mut m);
        m.remember("alpha bravo charlie", "resources/one", None, None, None).unwrap();
        m.remember("alpha bravo charlie", "resources/two", None, None, None).unwrap();
        let g = m.remember("alpha bravo charlie delta echo", "resources/three", None, None, None).unwrap();
        as_scope(&mut m, "room:b");
        for i in 0..40 {
            m.import_record(Kind::Memory, "resources/room", &format!("Room b dup {i}"), "alpha bravo charlie delta").unwrap();
        }
        m.set_scope_context(None, Some(vec!["room:c".into()]));
        let hits = m.recall("alpha bravo charlie", 3).unwrap();
        assert!(hits.iter().all(|h| h.scope.is_empty()), "{hits:?}");
        assert!(hits.iter().any(|h| h.uri == g), "the visible global record survives: {hits:?}");
        assert_eq!(hits.len(), 3, "{hits:?}");
    }

    /// An embedder that counts `embed_chunks` calls (the per-record embeddings).
    #[cfg(feature = "zvec")]
    struct CountingEmbedder(std::sync::atomic::AtomicUsize, crate::embedder::HashEmbedder);

    #[cfg(feature = "zvec")]
    impl crate::embedder::Embedder for CountingEmbedder {
        fn dim(&self) -> usize {
            self.1.dim()
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.1.embed(text)
        }
        fn embed_chunks(&self, text: &str) -> Vec<Vec<f32>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.embed_chunks(text)
        }
        fn name(&self) -> &'static str {
            "hash"
        }
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn invalidate_re_embeds_only_when_the_slot_text_changes() {
        let counter = std::sync::Arc::new(CountingEmbedder(Default::default(), crate::embedder::HashEmbedder::new()));
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        m.embedder = counter.clone();
        let embeds = || counter.0.load(Ordering::SeqCst);
        let uri = save_as(&mut m, "", "kestrel mounts: global procedure");
        save_as(&mut m, "room:a", "kestrel mounts: room a notes");
        as_full(&mut m);
        let n = embeds();
        // a cut that leaves each scope's remainder (a copy of the cut segment): no embedding
        assert_eq!(m.invalidate(&uri, now_ms() + 60_000).unwrap(), 2);
        assert_eq!(embeds(), n, "the remainders carry the text the slots already hold");
        assert!(slot_score(&m, "kestrel mounts: global procedure", &uri, "").unwrap() > 0.99);
        assert!(slot_score(&m, "kestrel mounts: room a notes", &uri, "room:a").unwrap() > 0.99);
        // a two-segment record whose newer segment is cut away entirely: the slot must follow
        // the older segment's text, so that one is re-embedded
        let t0 = now_ms() - 10_000;
        let seg = m.remember("lantern stock was forty", "resources/notes", Some(t0), None, None).unwrap();
        let t1 = now_ms() - 5_000;
        m.remember("lantern stock was forty", "resources/notes", Some(t1), None, None).unwrap();
        // (same text: same entity; now give the later segment another body)
        let e = m.store.newest_live(&seg, None).unwrap().unwrap();
        let mut later = e.clone();
        later.body = "lantern stock is twelve".into();
        later.valid_from_ms = t1;
        m.save_entry(&later).unwrap();
        let n = embeds();
        assert_eq!(m.invalidate(&seg, t1 - 1).unwrap(), 2, "both segments reach past the cut");
        assert_eq!(embeds(), n + 1, "the slot follows the older segment's text");
        assert!(slot_score(&m, "lantern stock was forty", &seg, "").unwrap() > 0.99);
    }

    #[cfg(feature = "zvec")]
    #[test]
    fn an_interrupted_slot_migration_resumes_after_its_checkpoint() {
        // Two shadowed uris in a pre-slot index; a checkpoint says the first is done (a run was
        // interrupted after it). The resumed migration does the second only, then records
        // completion and drops the checkpoint.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmtools-resume-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, vdir) = (dir.join("t.db"), dir.join("vectors").join("t"));
        let embed = crate::embedder::HashEmbedder::new();
        let (g_text, r_text) = ("operator curated steps", "room shadow draft");
        let (first, second) = {
            use crate::embedder::Embedder;
            let s = SqliteStore::open(&db).unwrap();
            let idx = crate::zvec_index::ZvecIndex::open(&vdir).unwrap();
            let mut uris = Vec::new();
            for title in ["aaa shadowed", "bbb shadowed"] {
                let uri = make_uri(SHARED_NS, Kind::Runbook, title);
                let mut e = Entry::new_now(uri.clone(), Kind::Runbook, SHARED_NS.into(), title.into(), g_text.into(), vec![], 60, uri.clone());
                s.put(&e).unwrap();
                e.scope = "room:a".into();
                e.body = r_text.into();
                s.put(&e).unwrap();
                idx.upsert_chunks(&uri, "", &embed.embed_chunks(r_text)).unwrap(); // 0.3.6: room text in the bare key
                uris.push(uri);
            }
            (uris[0].clone(), uris[1].clone())
        };
        assert!(first < second, "the work list runs in uri order");
        std::fs::write(slot_progress(&vdir), &first).unwrap();
        let m = LocalMemory::for_test_vec(SqliteStore::open(&db).unwrap(), &vdir);
        assert!(slot_score(&m, r_text, &first, "").unwrap() > 0.99, "checkpointed as done: not redone");
        assert!(slot_score(&m, g_text, &second, "").unwrap() > 0.99, "after the checkpoint: migrated");
        assert!(slot_score(&m, r_text, &second, "room:a").unwrap() > 0.99);
        assert!(slot_marker(&vdir).exists());
        assert!(!slot_progress(&vdir).exists(), "a finished migration leaves no checkpoint");
    }

    #[test]
    fn expansion_refuses_out_of_scope_bridges_no_via_leak() {
        // global seed -> private mid -> global far: a reader without the private scope must
        // not reach `far` THROUGH `mid`, and no rider's `via` may name `mid` - the traversal
        // gate exists because hydration-only filtering would leak the bridge node's title
        // through provenance (scope design, section 3.3).
        let mut m = LocalMemory::for_test(tmp_store());
        let seed = m.remember("Sigma anchor matches the query text", "resources/notes", None, None, None).unwrap();
        m.set_scope_context(Some("user:secret"), None); // write private, read everything (writer view)
        let mid = m.remember("Hidden bridge record", "resources/notes", None, None, None).unwrap();
        m.set_scope_context(Some(""), None);
        let far = m.remember("Distant global fact", "resources/other", None, None, None).unwrap();
        m.link(&seed, &mid, "links").unwrap();
        m.link(&mid, &far, "links").unwrap();
        // reader WITHOUT the private scope
        m.set_scope_context(Some(""), Some(vec!["room:x".to_string()]));
        let g = m.recall_expanded_graph("Sigma anchor matches", 2, 3).unwrap();
        assert!(g.riders.iter().all(|r| r.entry.uri != mid), "invisible node never rides");
        assert!(g.riders.iter().all(|r| r.entry.uri != far), "unreachable-except-through-invisible stays out");
        assert!(g.riders.iter().all(|r| r.via != mid), "no via provenance names the invisible node");
        assert!(g.links.iter().all(|e| e.from_uri != mid && e.to_uri != mid), "mini-subgraph never names it either");
        // the same reader WITH the scope sees the full chain
        m.set_scope_context(Some(""), Some(vec!["user:secret".to_string()]));
        let g = m.recall_expanded_graph("Sigma anchor matches", 3, 3).unwrap();
        assert!(g.riders.iter().any(|r| r.entry.uri == mid), "granted scope: bridge rides");
        assert!(g.riders.iter().any(|r| r.entry.uri == far), "granted scope: far end reachable through it");
    }

    #[test]
    fn expanded_graph_returns_links_internal_to_the_result_set() {
        let m = LocalMemory::for_test(tmp_store());
        // No lexical overlap between the query and the rider/outside bodies: the rider must
        // arrive via the edge only, regardless of the (process-global) recall-floor env state.
        let seed = m.remember("Delta anchor matches the query text", "resources/notes", None, None, None).unwrap();
        let rider = m.remember("Zeta companion joined by an edge only", "resources/notes", None, None, None).unwrap();
        let outside = m.remember("Unrelated island far away", "resources/other", None, None, None).unwrap();
        m.link(&seed, &rider, "informed").unwrap();
        m.link(&rider, &outside, "links").unwrap(); // one endpoint outside -> must not appear
        let g = m.recall_expanded_graph("Delta anchor matches", 2, 1).unwrap();
        assert!(g.riders.iter().any(|r| r.entry.uri == rider), "rider present");
        assert!(
            g.links.iter().any(|e| e.from_uri == seed && e.to_uri == rider && e.rel == "informed"),
            "seed->rider edge is in the mini-subgraph"
        );
        assert!(
            g.links.iter().all(|e| e.from_uri != outside && e.to_uri != outside),
            "edges leaving the result set are excluded"
        );
    }

    #[test]
    fn entity_kg_create_and_relate() {
        let m = LocalMemory::for_test(tmp_store());
        let vendor = m
            .import_record(Kind::Org, "resources/entities", "Contoso Hardware",
                &entity_body(Kind::Org, "Contoso Hardware", &[("role".into(), "principal".into()), ("sector".into(), "private".into())], ""))
            .unwrap();
        let proj = m
            .import_record(Kind::Engagement, "resources/entities", "Project Quillmoor",
                &entity_body(Kind::Engagement, "Project Quillmoor", &[("stage".into(), "BAU".into())], ""))
            .unwrap();
        let cx200 = m
            .import_record(Kind::Product, "resources/entities", "Contoso CX200", &entity_body(Kind::Product, "Contoso CX200", &[], ""))
            .unwrap();
        m.link(&cx200, &vendor, "made-by").unwrap();
        m.link(&proj, &cx200, "uses").unwrap();
        // the engagement reaches the product at 1 hop and the principal at 2 hops
        let n2 = m.neighbors(&[proj.clone()], 2, 10).unwrap();
        assert!(n2.contains(&cx200), "engagement -> product");
        assert!(n2.contains(&vendor), "engagement -> product -> principal");
        // the entity kind survives recall
        let hits = m.recall("Contoso CX200", 5).unwrap();
        assert!(hits.iter().any(|e| e.kind == Kind::Product && e.title == "Contoso CX200"));
    }

    // --- relevance floor: pure gate (no store, no env, deterministic) ---

    fn floor(abs_cosine: f64, abs_keyword: f64, rel_ratio: f64) -> crate::config::RecallFloor {
        crate::config::RecallFloor { enabled: true, abs_cosine, abs_keyword, rel_ratio }
    }
    fn v(uri: &str, c: f32) -> (String, f32) {
        (uri.to_string(), c)
    }
    fn kwh(uri: &str, s: f64) -> (String, f64) {
        (uri.to_string(), s)
    }

    #[test]
    fn floor_off_topic_below_cosine_injects_zero() {
        // off-topic query: every vector hit is below the absolute cosine floor.
        let vec = vec![v("a", 0.12), v("b", 0.08), v("c", 0.20)];
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45), 0);
        assert!(keep.is_empty(), "all cosines < 0.30 must inject nothing, got {keep:?}");
    }

    #[test]
    fn floor_adaptive_keeps_strong_drops_weak_tail() {
        // two strong hits then a steep drop-off: the relative ratio drops the tail, the absolute
        // floor is cleared by the strong ones. A 2-relevant query injects ~2, not the whole pool.
        let vec = vec![v("s1", 0.82), v("s2", 0.78), v("w1", 0.33), v("w2", 0.31)];
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45), 0);
        assert!(keep.contains("s1") && keep.contains("s2"), "strong hits kept");
        assert!(!keep.contains("w1") && !keep.contains("w2"), "weak tail dropped by ratio: {keep:?}");
    }

    #[test]
    fn floor_negative_top_cosine_does_not_admit_worse() {
        // all-negative cosine (off-topic): the relative clause is skipped (top <= 0) so it can't
        // invert into admitting worse hits; the absolute gate empties the channel.
        let vec = vec![v("a", -0.10), v("b", -0.20), v("c", -0.05)];
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45), 0);
        assert!(keep.is_empty(), "negative cosines must not survive, got {keep:?}");
    }

    #[test]
    fn floor_cosine_is_the_floor_in_hybrid_keyword_cannot_bypass() {
        // the leak fix: a junk record that shares a common WORD with the query (strong bm25) but is
        // semantically distant (low cosine) must NOT survive in hybrid - cosine is the floor, the
        // keyword channel cannot bypass it. The genuinely relevant hit (high cosine) survives.
        let kw = vec![kwh("kw_overlap_junk", 9.0), kwh("relevant", 3.0)];
        let vec = vec![v("relevant", 0.82), v("kw_overlap_junk", 0.18)];
        let keep = floor_survivors(&kw, &vec, &floor(0.30, 0.0, 0.45), 0);
        assert!(keep.contains("relevant"), "semantically-relevant hit survives");
        assert!(!keep.contains("kw_overlap_junk"), "shared-word junk with low cosine must be dropped: {keep:?}");
    }

    #[test]
    fn floor_keyword_only_mode_uses_bm25_relative_gate() {
        // no vector channel (keyword-only build / search failed): the bm25 relative gate is the
        // floor - the top match and anything within the ratio survive, the weak tail is dropped.
        let kw = vec![kwh("strong", 8.0), kwh("mid", 4.0), kwh("weak", 1.0)];
        let keep = floor_survivors(&kw, &[], &floor(0.30, 0.0, 0.45), 0); // 0.45*8 = 3.6
        assert!(keep.contains("strong") && keep.contains("mid"), "top + within-ratio kept");
        assert!(!keep.contains("weak"), "weak tail (1.0 < 3.6) dropped: {keep:?}");
    }

    #[test]
    fn floor_small_corpus_guard_rescues_a_fully_gated_tiny_pool() {
        // fresh/tiny corpus: IDF ~ 0, every -bm25 magnitude sits under the absolute floor.
        // With the whole pool rejected AND small enough to inject anyway, MATCH membership is
        // trusted and everything comes back.
        let kw = vec![kwh("a", 0.4), kwh("b", 0.3)];
        let f = floor(0.30, 2.0, 0.45); // abs_keyword 2.0 gates both
        assert!(floor_survivors(&kw, &[], &f, 0).is_empty(), "guard off (0): gate stands");
        let keep = floor_survivors(&kw, &[], &f, 6);
        assert_eq!(keep.len(), 2, "guard on: the whole tiny pool is rescued: {keep:?}");
    }

    #[test]
    fn floor_small_corpus_guard_does_not_fire_on_partial_survival_or_big_pools() {
        // one hit clears the floor -> the magnitudes are discriminating, gate stands
        let kw = vec![kwh("strong", 8.0), kwh("weak", 0.3)];
        let f = floor(0.30, 2.0, 0.01);
        let keep = floor_survivors(&kw, &[], &f, 6);
        assert!(keep.contains("strong") && !keep.contains("weak"), "partial survival: no rescue: {keep:?}");
        // pool bigger than the injectable window -> a big corpus, gate stands even if all-gated
        let big: Vec<(String, f64)> = (0..8).map(|i| kwh(&format!("r{i}"), 0.2)).collect();
        assert!(floor_survivors(&big, &[], &f, 6).is_empty(), "8 all-gated hits > max 6: no rescue");
        // hybrid mode (vector channel present): cosine is the floor, the guard is keyword-only
        let keep = floor_survivors(&kw, &[v("strong", 0.9)], &f, 6);
        assert!(keep.contains("strong") && !keep.contains("weak"), "hybrid path untouched by the guard");
    }

    // Serializes the few tests that mutate DM_RECALL_FLOOR (the other recall-calling tests assert
    // "contains X", which holds floor-on-or-off, so they don't need the lock).
    use crate::config::TEST_ENV_LOCK as RECALL_ENV_LOCK;

    #[test]
    fn floor_disabled_matches_prefloor_recall() {
        // The kill-switch guarantee: DM_RECALL_FLOOR=0 reproduces pre-floor recall exactly (the
        // disabled keyword path is plain store.recall). This is what rollback depends on.
        let _g = RECALL_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("DM_RECALL_FLOOR", "0");
        let m = LocalMemory::for_test(tmp_store());
        m.remember("alpha bravo charlie delta", "resources/notes", None, None, None).unwrap();
        m.remember("alpha only here", "resources/notes", None, None, None).unwrap();
        let disabled: std::collections::HashSet<String> =
            m.recall("alpha", 10).unwrap().into_iter().map(|e| e.uri).collect();
        let prefloor: std::collections::HashSet<String> =
            m.store.recall("alpha", 10).unwrap().into_iter().map(|e| e.uri).collect();
        std::env::remove_var("DM_RECALL_FLOOR");
        assert_eq!(disabled, prefloor, "floor-disabled recall must equal pre-floor (plain) recall");
        assert_eq!(disabled.len(), 2, "both keyword matches present when the floor is disabled");
    }

    #[test]
    fn floor_recent_sentinel_survives_in_both_modes() {
        // the empty/short-query sentinel (f64::INFINITY = recent() boot rows) is never floored out,
        // whether or not a vector channel is present.
        let kw = vec![kwh("recent1", f64::INFINITY), kwh("recent2", f64::INFINITY)];
        assert_eq!(floor_survivors(&kw, &[], &floor(0.30, 0.0, 0.45), 0).len(), 2, "keyword-only mode");
        // hybrid: a low-cosine vector pool would gate everything, but INFINITY recent rows still pass
        let vec = vec![v("x", 0.05)];
        let keep = floor_survivors(&kw, &vec, &floor(0.30, 0.0, 0.45), 0);
        assert!(keep.contains("recent1") && keep.contains("recent2"), "recent rows survive in hybrid too");
    }

    #[test]
    fn recall_in_filters_by_namespace_prefix() {
        let m = LocalMemory::for_test(tmp_store());
        m.import_record_at(Kind::Chunk, "ProjectAlpha/Plans/renovation.docx", "renovation §1 scope", "The renovation covers the reading room lighting and the archive shelving.", 1000, None).unwrap();
        m.import_record_at(Kind::Chunk, "ProjectBeta/Budget.xlsx", "budget §1", "The renovation budget lists reading room lighting and shelving.", 1000, None).unwrap();
        let all = m.recall_in("renovation lighting shelving", 10, &[]).unwrap();
        assert_eq!(all.len(), 2);
        let scoped = m.recall_in("renovation lighting shelving", 10, &["ProjectAlpha/".to_string()]).unwrap();
        assert_eq!(scoped.len(), 1);
        assert!(scoped[0].namespace.starts_with("ProjectAlpha/"));
        let none = m.recall_in("renovation lighting shelving", 10, &["ProjectGamma/".to_string()]).unwrap();
        assert!(none.is_empty());
    }

    // P0-5: a body or query the caller embedded before taking the tenant lock is used as is,
    // and the engine embeds nothing itself; a text the caller did not plan is still embedded
    // (under the lock), so the plan can never lose a vector.
    #[cfg(feature = "zvec")]
    #[test]
    fn pre_embedded_texts_are_used_and_the_engine_embeds_nothing() {
        use crate::embedder::DIM;
        let mut m = LocalMemory::for_test_vec(tmp_store(), &tmp_vdir());
        let body = "zpe pre embedded runbook body";
        // a marker vector the hash embedder would never produce for this body: finding the
        // record at cosine ~1 against it proves the handed-over vector was stored
        let mut marker = vec![0f32; DIM];
        marker[7] = 1.0;
        let mut pre = PreEmbedded::default();
        pre.chunks.insert(body.to_string(), vec![marker.clone()]);
        pre.queries.insert("zpe query".to_string(), m.embedder.embed(body));
        m.set_pre_embedded(pre);
        let uri = m.remember(body, "resources/notes", None, None, None).unwrap();
        assert_eq!(m.engine_embed_count(), 0, "the save embedded nothing itself");
        let hits = m.vindex.as_ref().unwrap().search(&marker, 5).unwrap();
        assert!(hits.iter().any(|h| h.uri == uri && h.score > 0.99), "the pre-embedded vector was stored: {hits:?}");
        let got = m.recall("zpe query", 5).unwrap();
        assert!(got.iter().any(|e| e.uri == uri), "recall used the pre-embedded query vector");
        assert_eq!(m.engine_embed_count(), 0, "the recall embedded nothing itself");
        // an unplanned text is embedded by the engine, as before
        m.clear_pre_embedded();
        m.remember("zpe unplanned note", "resources/notes", None, None, None).unwrap();
        assert_eq!(m.engine_embed_count(), 1);
    }

    // P0-4: the model requirement refuses the placeholder embedder (and a build without one);
    // any real model passes, and opted out (DM_REQUIRE_MODEL=0) nothing is refused.
    #[test]
    fn model_requirement_refuses_only_the_placeholder_when_required() {
        assert!(check_model_requirement(Some("hash"), false).is_ok());
        assert!(check_model_requirement(None, false).is_ok());
        let e = check_model_requirement(Some("hash"), true).unwrap_err();
        assert!(format!("{e:#}").contains("placeholder hash embedder"), "{e:#}");
        assert!(format!("{:#}", check_model_requirement(None, true).unwrap_err()).contains("no embedding model"));
        for real in ["candle-bge-small", "candle-custom", "fastembed-bge-small", "model2vec"] {
            assert!(check_model_requirement(Some(real), true).is_ok(), "{real}");
        }
    }

    // The refusal is one error that says it all: the model, the cache dmem looked in, how to
    // pre-install the model, and the escape hatch; a build without a model says that instead.
    #[test]
    fn the_model_refusal_names_the_model_the_cache_the_fix_and_the_escape_hatch() {
        let e = format!("{:#}", check_model_requirement(Some("hash"), true).unwrap_err());
        let d = crate::embedder::active_embedder_diag();
        match d.model_id {
            Some(m) => {
                assert!(e.contains(&format!("the embedding model {m} did not load")), "{e}");
                let cache = d.cache_dir.expect("a cache dir").display().to_string();
                assert!(e.contains(&format!("Model cache: {cache} (")), "{e}");
                assert!(e.contains("`dmem doctor`") && e.contains("README: Offline / air-gapped"), "{e}");
            }
            None => assert!(e.contains("this dmem build has no embedding model"), "{e}"),
        }
        assert!(e.contains("DM_REQUIRE_MODEL is set to 0"), "{e}");
        let e = format!("{:#}", check_model_requirement(None, true).unwrap_err());
        assert!(e.contains("this dmem build has no embedding model") && e.contains("DM_REQUIRE_MODEL is set to 0"), "{e}");
    }

    /// An embedder that answers every text with the same vector.
    #[cfg(feature = "zvec")]
    struct FixedEmbedder(Vec<f32>);

    #[cfg(feature = "zvec")]
    impl crate::embedder::Embedder for FixedEmbedder {
        fn dim(&self) -> usize {
            crate::embedder::DIM
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            self.0.clone()
        }
        fn name(&self) -> &'static str {
            "candle-custom"
        }
    }

    // A model that loads but is the wrong one (another width, a tokenizer that does not match
    // its weights, so every embed fails into a zero vector) does not pass as loaded.
    #[cfg(feature = "zvec")]
    #[test]
    fn the_probe_refuses_a_model_that_loads_but_embeds_wrong() {
        use crate::embedder::DIM;
        let mut ok = vec![0.0; DIM];
        ok[7] = 1.0;
        assert_eq!(probe_embedder(&FixedEmbedder(ok.clone())), Ok(()));
        assert_eq!(probe_embedder(&crate::embedder::HashEmbedder::new()), Ok(()));
        let wide = probe_embedder(&FixedEmbedder(vec![0.1; 768])).unwrap_err();
        assert!(wide.starts_with("candle-custom: ") && wide.contains(&format!("as a 768-d vector; dmem needs a non-zero {DIM}-d vector")), "{wide}");
        let zero = probe_embedder(&FixedEmbedder(vec![0.0; DIM])).unwrap_err();
        assert!(zero.contains("as a zero vector"), "{zero}");
        ok[3] = f32::NAN;
        let nan = probe_embedder(&FixedEmbedder(ok)).unwrap_err();
        assert!(nan.contains("non-finite values"), "{nan}");
    }

    // P0-4: a vector index that cannot open degrades to keyword-only recall, unless
    // DM_REQUIRE_VECTORS is set: then the open fails with `VectorIndexRequired`.
    #[cfg(feature = "zvec")]
    #[test]
    fn a_broken_vector_index_fails_the_open_when_vectors_are_required() {
        let store = tmp_store();
        let vdir = tmp_vdir();
        std::fs::create_dir_all(vdir.parent().unwrap()).unwrap();
        std::fs::write(&vdir, "not a vector index").unwrap(); // a file where the index dir belongs
        let embedder = crate::embedder::HashEmbedder::new();
        assert!(open_vector_index(&store, &vdir, &embedder, false, false).unwrap().is_none(), "keyword-only, as before");
        let Err(e) = open_vector_index(&store, &vdir, &embedder, true, false) else { panic!("the open must fail") };
        assert!(e.downcast_ref::<VectorIndexRequired>().is_some(), "{e:#}");
        assert!(format!("{e:#}").contains("DM_REQUIRE_VECTORS"), "{e:#}");
    }

    // The typed saves store exactly the body their pure builder returns (what a caller
    // pre-embeds), for every kind the server plans.
    #[test]
    fn typed_save_bodies_match_their_builders() {
        let m = LocalMemory::for_test(tmp_store());
        let body_of = |uri: String| m.store.get(&uri).unwrap().unwrap().body;
        assert_eq!(body_of(m.log_decision("t1", "c", "d", "r", "resources/notes", None).unwrap()), decision_body("t1", "c", "d", "r"));
        assert_eq!(body_of(m.log_lesson("t2", "l", "agent/lessons", None).unwrap()), lesson_body("t2", "l"));
        assert_eq!(body_of(m.log_incident("t3", "i", "r", "resources/incidents", None).unwrap()), incident_body("t3", "i", "r"));
        assert_eq!(body_of(m.add_reminder("t4", "x", "agent/reminders", None).unwrap()), reminder_body("t4", "x"));
        assert_eq!(body_of(m.log_runbook("t5", "s", "resources/runbooks", None).unwrap()), runbook_body("t5", "s"));
        assert_eq!(body_of(m.log_convention("t6", "r", "resources/conventions", None).unwrap()), convention_body("t6", "r"));
    }
}

/// Step B: the recall-floor CALIBRATION harness. Dev-only and feature-gated to `candle` (the
/// production bge-small embedder), so it NEVER compiles into the release binary. It stands up a
/// real-embeddings store in an isolated temp dir (never a live tenant database), seeds a labeled
/// synthetic corpus (topic clusters + vector-only paraphrases with zero keyword overlap +
/// hard-negatives that share a keyword but are off-topic + fillers), sweeps (abs_cosine, rel_ratio)
/// on TRAIN queries, and validates the chosen thresholds on a HELD-OUT split. Run it with:
///   cargo test --features candle floor_eval -- --nocapture
/// then read the printed RECOMMEND line and bake it into `RecallFloor::DEFAULTS`.
#[cfg(all(test, feature = "candle"))]
mod floor_eval {
    use super::*;
    use std::collections::{HashMap, HashSet};

    use crate::config::TEST_ENV_LOCK as ENV_LOCK;

    // (label, text). Clusters share vocabulary; VO* are vector-only paraphrases of the VQ query
    // with NO shared content word; H* are hard-negatives (share a keyword with a query, off-topic);
    // F* are unrelated fillers.
    fn corpus() -> Vec<(&'static str, &'static str)> {
        vec![
            ("A1", "The postgres database server was OOM-killed during the data migration."),
            ("A2", "Postgres ran out of memory mid-migration and the kernel killed the process."),
            ("A3", "We set work_mem too high and postgres exhausted RAM during the bulk import migration."),
            ("A4", "The migration batch size was too large; postgres memory ballooned until it crashed."),
            ("A5", "After the OOM kill we tuned shared_buffers and lowered the migration batch size."),
            ("A6", "Adding swap stopped the postgres migration from being OOM-killed again."),
            ("B1", "The kubernetes ingress controller failed to renew its TLS certificate."),
            ("B2", "cert-manager could not issue a Let's Encrypt certificate for the ingress."),
            ("B3", "The nginx ingress served an expired certificate after the renewal hook failed."),
            ("B4", "We fixed ingress TLS by reconfiguring the cert-manager ClusterIssuer."),
            ("B5", "Ingress traffic broke because the TLS certificate secret was not mounted."),
            ("B6", "The k8s ingress returned 503 until the TLS secret was regenerated."),
            ("C1", "Submitted the Quillmoor Library grant application to the Northwind Arts Council."),
            ("C2", "The Northwind Arts Council grant requires Quillmoor Library to match a quarter of the funding."),
            ("C3", "Two local schools wrote letters of support for the Quillmoor Library grant application."),
            ("C4", "The arts council grant checklist for Quillmoor Library asked for audited accounts and a charity number."),
            ("C5", "Quillmoor Library won the arts council grant and the reading-room project moved into delivery."),
            ("C6", "Drafted the budget and timeline sections of the Quillmoor Library grant application."),
            // vector-only: semantically about an overload outage, ZERO content-word overlap with VQ
            ("VO1", "Users hit timeouts everywhere once the peak hour rush arrived."),
            ("VO2", "Every request started failing as concurrency climbed past the limit."),
            // hard-negatives: share a keyword with the postgres query but off-topic
            ("H1", "A memory foam mattress review covering comfort, firmness, and price."),
            ("H2", "The seasonal migration of shorebirds across the peninsula peaks in October."),
            ("H3", "A certificate of attendance for the training was emailed to all staff."),
            // fillers
            ("F1", "Reorganized the home lab rack and labeled every network cable."),
            ("F2", "Upgraded the NAS to ZFS and enabled nightly snapshots."),
            ("F3", "Notes on the history of the Roman aqueducts and their engineering."),
            ("F4", "Wrote a shell script to rotate and compress old log files."),
            ("F5", "Planned the quarterly budget for the storage refresh."),
            ("F6", "A recipe for sourdough bread with a long cold ferment."),
            ("F7", "Benchmarked NVMe drives for random read IOPS."),
            ("F8", "Set up Grafana dashboards for the Proxmox cluster."),
            ("F9", "Reviewed firewall rules for the DMZ segment."),
            ("F10", "Configured Wireguard tunnels between the two sites."),
            ("F11", "Tested backup restore from the offsite repository."),
            ("F12", "Compared two espresso machines for a small office pantry."),
        ]
    }

    // (query, relevant labels). Empty relevant set = deliberately off-topic (must inject 0).
    fn train() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("postgres database ran out of memory and was OOM killed during the migration",
                vec!["A1", "A2", "A3", "A4", "A5", "A6"]),
            ("kubernetes ingress failed to renew its TLS certificate",
                vec!["B1", "B2", "B3", "B4", "B5", "B6"]),
            ("the rules and format of test match cricket", vec![]),
        ]
    }
    fn heldout() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("Quillmoor Library grant application submission to the arts council",
                vec!["C1", "C2", "C3", "C4", "C5", "C6"]),
            ("the production cluster became unresponsive when traffic surged",
                vec!["VO1", "VO2"]),
            ("lattice gauge theory in quantum chromodynamics", vec![]),
        ]
    }

    const POOL: usize = 24;

    struct QChannels {
        kw: Vec<(String, f64)>,
        vec: Vec<(String, f32)>,
        rel: HashSet<String>,
        pool: HashSet<String>,
    }

    /// recall-retained (over relevant present in pool), junk-survivor count, total survivors.
    fn eval_query(q: &QChannels, f: &config::RecallFloor) -> (f64, usize, usize) {
        let keep = floor_survivors(&q.kw, &q.vec, f, 0);
        let rel_in_pool: usize = q.rel.iter().filter(|u| q.pool.contains(*u)).count();
        let rel_kept = keep.iter().filter(|u| q.rel.contains(*u)).count();
        let junk_kept = keep.len() - rel_kept;
        let retained = if rel_in_pool == 0 { 1.0 } else { rel_kept as f64 / rel_in_pool as f64 };
        (retained, junk_kept, keep.len())
    }

    #[test]
    fn floor_eval_calibrate() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("dmeval-{}-{}", std::process::id(), now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("DM_DATA_DIR", &dir);
        let m = LocalMemory::open_tenant("eval").expect("open eval tenant");
        // HIGH-2 guard: a silent HashEmbedder fallback would make every cosine number bogus.
        assert_ne!(m.embedder.name(), "hash", "calibration needs a real embedder; build --features candle");
        eprintln!("\nEVAL embedder = {}  (dim {})", m.embedder.name(), m.embedder.dim());

        let mut uri_of: HashMap<&str, String> = HashMap::new();
        for (label, text) in corpus() {
            uri_of.insert(label, m.remember(text, "resources/eval", None, None, None).unwrap());
        }
        let channels = |qs: Vec<(&'static str, Vec<&'static str>)>| -> Vec<QChannels> {
            qs.into_iter()
                .map(|(q, labels)| {
                    let kw = m.store.recall_scored(q, POOL).unwrap();
                    let qv = m.embedder.embed(q);
                    let vec: Vec<(String, f32)> =
                        m.vindex.as_ref().unwrap().search(&qv, POOL).unwrap().into_iter().map(|h| (h.uri, h.score)).collect();
                    let rel: HashSet<String> = labels.iter().map(|l| uri_of[*l].clone()).collect();
                    let mut pool: HashSet<String> = kw.iter().map(|(e, _)| e.uri.clone()).collect();
                    pool.extend(vec.iter().map(|(u, _)| u.clone()));
                    QChannels { kw: kw.iter().map(|(e, s)| (e.uri.clone(), *s)).collect(), vec, rel, pool }
                })
                .collect()
        };
        let train = channels(train());
        let held = channels(heldout());

        // raw cosine separation on train (relevant vs junk), to see the gap the floor exploits
        let (mut rc, mut jc) = (Vec::new(), Vec::new());
        for q in &train {
            for (u, c) in &q.vec {
                if q.rel.contains(u) { rc.push(*c) } else { jc.push(*c) }
            }
        }
        rc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        jc.sort_by(|a, b| b.partial_cmp(a).unwrap());
        eprintln!("EVAL train cosine: relevant min={:.3} (all={:?})", rc.first().copied().unwrap_or(0.0),
            rc.iter().map(|x| format!("{x:.2}")).collect::<Vec<_>>());
        eprintln!("EVAL train cosine: junk top5={:?}", jc.iter().take(5).map(|x| format!("{x:.2}")).collect::<Vec<_>>());
        // off-topic queries set the leak threshold: their top cosine is the bar abs_cosine must clear
        for q in train.iter().filter(|q| q.rel.is_empty()) {
            let mut tops: Vec<f32> = q.vec.iter().map(|(_, c)| *c).collect();
            tops.sort_by(|a, b| b.partial_cmp(a).unwrap());
            eprintln!("EVAL off-topic top5 cosine={:?}", tops.iter().take(5).map(|x| format!("{x:.2}")).collect::<Vec<_>>());
        }

        // Sweep (abs_cosine, rel_ratio). PRIORITY: off-topic-injects-zero is the hard guarantee
        // (leak==0), THEN maximize recall-retained, THEN minimize noise, THEN lower abs_cosine
        // (more conservative on recall). Perfect separation is impossible (relevant/junk cosine
        // bands overlap), so we take the best tradeoff under the zero-leak constraint.
        let mut best: Option<(f64, f64, f64, usize)> = None; // (ac, rr, retained, junk)
        for ac_i in 10..=80 {
            let ac = ac_i as f64 / 100.0;
            for rr_i in (20..=70).step_by(5) {
                let rr = rr_i as f64 / 100.0;
                let f = config::RecallFloor { enabled: true, abs_cosine: ac, abs_keyword: 0.0, rel_ratio: rr };
                let (mut ret_sum, mut ret_n, mut junk, mut leak) = (0.0, 0, 0usize, 0usize);
                for q in &train {
                    let (ret, jk, tot) = eval_query(q, &f);
                    if q.rel.is_empty() { leak += tot } else { ret_sum += ret; ret_n += 1; junk += jk; }
                }
                if leak != 0 { continue; }
                let retained = ret_sum / ret_n as f64;
                let better = match best {
                    None => true,
                    Some((bac, _, bret, bj)) => {
                        retained > bret + 1e-9
                            || ((retained - bret).abs() < 1e-9 && junk < bj)
                            || ((retained - bret).abs() < 1e-9 && junk == bj && ac < bac)
                    }
                };
                if better { best = Some((ac, rr, retained, junk)); }
            }
        }
        let (ac, rr, ret, _j) = best.expect("no abs_cosine achieved zero off-topic leak (off-topic top cosine too high)");
        let chosen = config::RecallFloor { enabled: true, abs_cosine: ac, abs_keyword: 0.0, rel_ratio: rr };
        eprintln!("EVAL RECOMMEND  abs_cosine={ac:.2}  rel_ratio={rr:.2}  abs_keyword=0.0  (train recall-retained={ret:.2})");

        let _ = chosen; // the sweep RECOMMEND is informational; we ship + validate DEFAULTS below.

        // Validate the SHIPPED defaults on held-out (regression guard on the baked-in constants).
        // HARD guarantees: off-topic injects ZERO; named-entity cluster recall stays high; the
        // zero-keyword-overlap paraphrase still survives the cosine floor (semantic recall).
        let ship = config::RecallFloor::DEFAULTS;
        eprintln!("EVAL held-out @ SHIPPED defaults (abs_cosine={:.2} rel_ratio={:.2}):", ship.abs_cosine, ship.rel_ratio);
        for (q, labels) in heldout() {
            let qc = held.iter().find(|c| c.rel == labels.iter().map(|l| uri_of[*l].clone()).collect::<HashSet<_>>()).unwrap();
            let (ret, jk, tot) = eval_query(qc, &ship);
            let kind = if labels.is_empty() { "off-topic" } else if labels.iter().all(|l| l.starts_with("VO")) { "vector-only" } else { "cluster" };
            eprintln!("  [{kind}] q={q:?}  retained={ret:.2} survivors={tot} junk={jk}");
            if labels.is_empty() {
                assert_eq!(tot, 0, "off-topic must inject zero at shipped defaults, got {tot}");
            } else if kind == "cluster" {
                assert!(ret >= 0.8, "named-entity cluster recall fell to {ret:.2} at shipped defaults");
            } else if kind == "vector-only" {
                assert!(ret > 0.0, "shipped cosine floor must keep >=1 zero-keyword-overlap paraphrase");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("DM_DATA_DIR");
    }
}
