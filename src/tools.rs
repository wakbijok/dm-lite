//! High-level memory API: the typed guided save tools (per-kind required-field
//! validation) + recall. This is daimon's distinctive layer over the engine.

use crate::config;
use crate::entry::{make_uri, now_ms, Edge, Entry, Kind};
use crate::sqlite::SqliteStore;
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
    #[cfg(feature = "zvec")]
    vindex: Option<crate::zvec_index::ZvecIndex>,
    #[cfg(feature = "zvec")]
    embedder: std::sync::Arc<dyn crate::embedder::Embedder>,
}

/// The embedder, loaded ONCE per process and shared (Arc). Loading the model is expensive and
/// the server opens a tenant store per request, so caching it keeps recall fast and the daemon's
/// RSS stable (~200MB warm) instead of re-mmapping per request. The daemon is a managed service
/// (launchd / systemd): its RAM is reclaimed by STOPPING the service, not by in-process eviction
/// (macOS does not return freed model memory to the OS anyway - verified).
#[cfg(feature = "zvec")]
fn make_embedder() -> std::sync::Arc<dyn crate::embedder::Embedder> {
    use std::sync::{Arc, OnceLock};
    static EMBEDDER: OnceLock<Arc<dyn crate::embedder::Embedder>> = OnceLock::new();
    EMBEDDER.get_or_init(build_embedder).clone()
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
            if d.cache_present { "cached" } else { "will download on first use (needs network)" },
        );
    }
    let _ = make_embedder();
}

/// Construct the best available embedder (called once, behind the `make_embedder` cache).
#[cfg(feature = "zvec")]
fn build_embedder() -> std::sync::Arc<dyn crate::embedder::Embedder> {
    use std::sync::Arc;
    #[cfg(feature = "fastembed")]
    {
        match crate::embedder::FastEmbedder::new() {
            Ok(e) => return Arc::new(e),
            Err(err) => eprintln!("dmem: fastembed model unavailable ({err:#}); using placeholder embedder"),
        }
    }
    #[cfg(all(feature = "candle", not(feature = "fastembed")))]
    {
        match crate::embedder::CandleEmbedder::new() {
            Ok(e) => return Arc::new(e),
            Err(err) => eprintln!("dmem: candle model unavailable ({err:#}); using placeholder embedder"),
        }
    }
    #[cfg(all(feature = "model2vec", not(feature = "fastembed"), not(feature = "candle")))]
    {
        match crate::embedder::Model2VecEmbedder::new() {
            Ok(e) => return Arc::new(e),
            Err(err) => eprintln!("dmem: model2vec model unavailable ({err:#}); using placeholder embedder"),
        }
    }
    Arc::new(crate::embedder::HashEmbedder::new())
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
                let name = rest[..j].trim();
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
/// stripped ("Izuhomeland (Windows)" -> "Izuhomeland"), and each `/`-separated variant.
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
pub(crate) fn entity_body(kind: Kind, name: &str, attrs: &[(String, String)], desc: &str) -> String {
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
        // Small-corpus guard (TencentDB-Agent-Memory study, 09-08-2026): BM25 absolute
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

impl LocalMemory {
    /// Open the embedded-mode tenant ($DM_TENANT, else "default").
    pub fn open() -> Result<Self> {
        Self::open_tenant(&config::tenant())
    }

    /// Open a specific tenant's store explicitly. Server mode uses this per request so it
    /// never mutates the process-global $DM_TENANT (which would race under concurrency).
    pub fn open_tenant(tenant: &str) -> Result<Self> {
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
        {
            let vdir = config::vector_dir(tenant)?;
            let vindex = match crate::zvec_index::ZvecIndex::open(&vdir) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("dmem: zvec vector index unavailable ({:#}); falling back to keyword-only recall", e);
                    None
                }
            };
            return Ok(Self { store, write_scope, agent_view: None, vindex, embedder: make_embedder() });
        }
        #[cfg(not(feature = "zvec"))]
        Ok(Self { store, write_scope, agent_view: None })
    }

    /// Rebind this engine handle to a reader/writer scope context (scope primitive). Server
    /// mode calls this per request under the tenant lock, from the TOKEN's grants; tests use
    /// it directly. `write_scope`: Some("") = global writer, Some(s) = scoped writer, None =
    /// writes rejected (decision Q3). `read_scopes` None = full tenant.
    pub fn set_scope_context(&mut self, write_scope: Option<&str>, read_scopes: Option<Vec<String>>) {
        self.write_scope = write_scope.map(str::to_string);
        self.store.set_read_scopes(read_scopes);
    }

    /// Bind this handle to an agent identity's view (persona-tree protection). Server mode
    /// sets it per request from the TOKEN's agent label; None = agent-less.
    pub fn set_agent_view(&mut self, agent: Option<String>) {
        self.agent_view = agent.clone();
        self.store.set_agent_view(agent);
    }

    /// Writes into the `agents/` tree may only target the caller's own subtree. Everything
    /// outside `agents/` is the shared pool (by design). Agent-less identities: no restriction.
    fn guard_agent_namespace(&self, namespace: &str) -> Result<()> {
        let Some(owner) = crate::entry::split_agents_tree(namespace) else {
            return Ok(());
        };
        match &self.agent_view {
            Some(a) if owner.eq_ignore_ascii_case(a) => Ok(()),
            Some(_) => anyhow::bail!("namespace '{namespace}' belongs to another agent identity"),
            // No agent label: only a FULLY unrestricted identity may touch the agents/ tree.
            // A scoped principal is a user, not an agent - without this, a scoped token could
            // plant records in an agent's tree that the (typically full-tenant) agent token
            // then reads as its own persona (caught by the 0.3.2 live release smoke).
            None if !self.write_restricted() => Ok(()),
            None => anyhow::bail!("scoped identities cannot write into the agents/ tree"),
        }
    }

    /// Is this identity anything less than a full-tenant writer? (Scoped or write-denied.)
    fn write_restricted(&self) -> bool {
        self.write_scope.as_deref() != Some("")
    }

    /// The write scope for a save, or an error for a write-denied identity (decision Q3: a
    /// scope-bound token without a write scope cannot write; promotion to global is explicit).
    fn effective_write_scope(&self) -> Result<&str> {
        self.write_scope
            .as_deref()
            .ok_or_else(|| anyhow!("this identity has no write scope (writes are read-only for this token)"))
    }

    /// Guard for retracting mutations (forget/invalidate). A scope-restricted writer may only
    /// mutate records IN ITS OWN write scope; an agent-labeled identity may not touch another
    /// agent's `agents/<b>/...` records (the target must be VISIBLE - get() applies both the
    /// scope and agent-tree gates, so a foreign persona reads as absent here).
    fn guard_mutation_target(&self, uri: &str) -> Result<()> {
        if !self.write_restricted() && self.agent_view.is_none() {
            return Ok(());
        }
        let e = self
            .store
            .get(uri)?
            .ok_or_else(|| anyhow!("record not found (or not visible to this identity)"))?;
        if self.write_restricted() {
            let ws = self.effective_write_scope()?;
            if e.scope != ws {
                anyhow::bail!("record is outside this identity's write scope");
            }
        }
        Ok(())
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

    /// Put an entry and (under zvec) embed its body. Fail-open: a vector-index hiccup never
    /// blocks the canonical SQLite save. Bitemporal invariant: the hashed-PK upsert overwrites
    /// the prior vector, so the index holds exactly the current valid version.
    fn save_entry(&self, e: &Entry) -> Result<()> {
        self.store.put(e)?;
        #[cfg(feature = "zvec")]
        if let Some(vindex) = &self.vindex {
            let chunks = self.embedder.embed_chunks(&e.body);
            if let Err(err) = vindex.upsert_chunks(&e.uri, &chunks) {
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
            vindex.upsert_chunks(&e.uri, &chunks)?;
        }
        Ok((all.len(), long))
    }

    /// Import a record preserving its ORIGINAL creation/valid time (for v1->v2 migration).
    /// System time stays "now" (when we recorded it); valid/created time is the original.
    pub fn import_record_at(&self, kind: Kind, namespace: &str, title: &str, body: &str, created_ms: i64, importance: Option<i64>) -> Result<String> {
        require(title, "title")?;
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
        self.save_entry(&e)?;
        Ok(uri)
    }

    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(decision, "decision")?;
        let body = format!(
            "# {}\n\n**Context:** {}\n\n**Decision:** {}\n\n**Rationale:** {}\n",
            title, context, decision, rationale
        );
        self.save(Kind::Decision, namespace, title, body, 70, vec!["decision".into()], author)
    }

    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(lesson, "lesson")?;
        let body = format!("# {}\n\n**Lesson:** {}\n", title, lesson);
        self.save(Kind::AgentLesson, namespace, title, body, 60, vec!["agent_lesson".into()], author)
    }

    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(impact, "impact")?;
        let body = format!(
            "# {}\n\n**Impact:** {}\n\n**Resolution:** {}\n",
            title, impact, resolution
        );
        self.save(Kind::IncidentSummary, namespace, title, body, 65, vec!["incident_summary".into()], author)
    }

    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>, author: Option<&str>) -> Result<String> {
        require(text, "text")?;
        let title = first_line(text);
        self.save_valid(Kind::Memory, namespace, &title, text.to_string(), 50, vec![], valid_from, valid_to, author)
    }

    /// Application-time invalidation: this entity's fact is no longer true from `valid_to_ms` on.
    pub fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        self.guard_mutation_target(uri)?;
        self.store.invalidate(uri, valid_to_ms)
    }

    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(text, "text")?;
        let body = format!("# {}\n\n**Reminder:** {}\n", title, text);
        self.save(Kind::Reminder, namespace, title, body, 55, vec!["reminder".into()], author)
    }

    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(steps, "steps")?;
        let body = format!("# {}\n\n**Runbook:** {}\n", title, steps);
        self.save(Kind::Runbook, namespace, title, body, 60, vec!["runbook".into()], author)
    }

    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str, author: Option<&str>) -> Result<String> {
        require(title, "title")?;
        require(rule, "rule")?;
        let body = format!("# {}\n\n**Convention:** {}\n", title, rule);
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

    /// Hybrid recall: SQLite FTS (keyword) + zvec (dense vector), fused by RRF, then nudged
    /// by runtime signals.
    #[cfg(feature = "zvec")]
    fn recall_hybrid(&self, query: &str, limit: usize, vindex: &crate::zvec_index::ZvecIndex) -> Result<Vec<Entry>> {
        use std::collections::HashMap;
        let pool = (limit * 2).max(10);
        // Pull both channels WITH their magnitudes: keyword hits carry -bm25 (and arrive already
        // hydrated), vector hits carry cosine similarity.
        let kw_scored = self.store.recall_scored(query, pool)?;
        let kw: Vec<(String, f64)> = kw_scored.iter().map(|(e, s)| (e.uri.clone(), *s)).collect();
        let qv = self.embedder.embed(query);
        let vec: Vec<(String, f32)> = match vindex.search(&qv, pool) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("dmem: vector search failed ({e:#}); using keyword results only");
                Vec::new()
            }
        };

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
        // Hydrate, then apply the modest runtime-signal multiplier AFTER RRF: a bounded
        // (<=1.25x) nudge that reorders near-equal scores without overturning a clear gap.
        // Keyword survivors are already hydrated (recall_scored returned Entry); fetch only
        // vector-only survivors via get().
        let now = now_ms();
        let mut entries: HashMap<String, Entry> = kw_scored.into_iter().map(|(e, _)| (e.uri.clone(), e)).collect();
        let uris: Vec<String> = score.keys().cloned().collect();
        let sigs = self.store.read_signals(&uris).unwrap_or_default();
        let mut scored: Vec<(Entry, f64)> = Vec::new();
        for (uri, rrf) in score {
            let e = match entries.remove(&uri) {
                Some(e) => e,
                None => match self.store.get(&uri)? {
                    Some(e) => e,
                    None => continue,
                },
            };
            // Skills surface via the ~/.claude/skills projection, not recall. The keyword channel
            // already excludes them in SQL; the vector channel can still return a skill uri, so drop
            // it here before it pollutes per-prompt context with a full SKILL.md body.
            if e.kind == crate::entry::Kind::Skill {
                continue;
            }
            let (ac, la) = sigs.get(&uri).copied().unwrap_or((0, 0));
            let s = rrf * signal_boost(e.importance, ac, la, now);
            scored.push((e, s));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let out: Vec<Entry> = scored.into_iter().take(limit).map(|(e, _)| e).collect();
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

    /// Re-rank keyword hits by their FTS order (base = 1/(1+rank)), gently nudged by the
    /// runtime-signal multiplier (<=1.25x). The base dominates at the top, so a clearly
    /// higher-ranked hit is not displaced; adjacent items at deeper ranks may reorder.
    fn rescore_keyword(&self, hits: Vec<Entry>, limit: usize) -> Vec<Entry> {
        let now = now_ms();
        let uris: Vec<String> = hits.iter().map(|e| e.uri.clone()).collect();
        let sigs = self.store.read_signals(&uris).unwrap_or_default();
        let mut scored: Vec<(Entry, f64)> = hits
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let base = 1.0 / (1.0 + i as f64);
                let (ac, la) = sigs.get(&e.uri).copied().unwrap_or((0, 0));
                let s = base * signal_boost(e.importance, ac, la, now);
                (e, s)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit).map(|(e, _)| e).collect()
    }

    /// Best-effort: bump the access signals for the recalled set in one transaction.
    /// Never fails recall.
    fn bump_recalled(&self, entries: &[Entry]) {
        let uris: Vec<&str> = entries.iter().map(|e| e.uri.as_str()).collect();
        let _ = self.store.bump_signals(&uris, now_ms());
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

    /// Retract a uri: drop it from recall (close current version, keep lineage) and remove
    /// its vector. Returns how many current versions were closed.
    pub fn forget(&self, uri: &str) -> Result<usize> {
        self.guard_mutation_target(uri)?;
        let n = self.store.forget(uri)?;
        #[cfg(feature = "zvec")]
        if let Some(vindex) = &self.vindex {
            let _ = vindex.remove(uri); // best-effort; source-of-truth is the SQLite close
        }
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
        self.guard_edge_endpoints(from_uri, to_uri)?;
        self.store.link(from_uri, to_uri, rel)
    }
    pub fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        self.guard_edge_endpoints(from_uri, to_uri)?;
        self.store.unlink(from_uri, to_uri, rel)
    }

    /// Edge mutations for a restricted identity require BOTH endpoints to be live and visible
    /// (edges carry no content, so read visibility is the right bar - a scoped principal may
    /// link its own record to a global entity, but never touch edges of records it cannot see).
    fn guard_edge_endpoints(&self, from_uri: &str, to_uri: &str) -> Result<()> {
        if !self.write_restricted() && self.agent_view.is_none() {
            return Ok(());
        }
        self.effective_write_scope()?; // write-denied identities cannot mutate edges either
        for u in [from_uri, to_uri] {
            if self.store.get(u)?.is_none() {
                anyhow::bail!("edge endpoint not found (or not visible to this identity): {u}");
            }
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
        let mut linked = 0usize;
        for e in &records {
            for name in parse_wikilinks(&e.body) {
                let slug = crate::entry::slug(&name);
                if slug.is_empty() {
                    continue;
                }
                if let Some(target) = by_slug.get(&slug) {
                    if target != &e.uri {
                        self.store.link(&e.uri, target, "links")?;
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
                vindex: None,
                embedder: std::sync::Arc::new(crate::embedder::HashEmbedder::new()),
            }
        }
        #[cfg(not(feature = "zvec"))]
        {
            Self { store, write_scope: Some(String::new()), agent_view: None }
        }
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
            return Ok(Memory::Remote(crate::client::RemoteClient::new(link)?));
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
        stamp_author(&mut tags, Some("izu"));
        assert!(tags.contains(&"author:izu".to_string()));
        // a later write path with a different agent must not re-assign attribution
        stamp_author(&mut tags, Some("devin"));
        assert_eq!(tags.iter().filter(|t| t.starts_with("author:")).count(), 1);
        assert!(tags.contains(&"author:izu".to_string()), "first attribution wins");
        // agent-less: byte-identical tags
        let mut plain = vec!["reminder".to_string()];
        stamp_author(&mut plain, None);
        assert_eq!(plain, vec!["reminder".to_string()]);
    }

    #[test]
    fn authenticated_agent_writes_carry_author_attribution() {
        let m = LocalMemory::for_test(tmp_store());
        let uri = m.log_decision("Pick zvec", "", "zvec it is", "", "resources/notes", Some("izu")).unwrap();
        let hit = m.recall("pick zvec", 5).unwrap().into_iter().find(|e| e.uri == uri).expect("recalled");
        assert!(hit.tags.contains(&"author:izu".to_string()), "typed save stamps the author: {:?}", hit.tags);

        let uri = m.remember("the bridge runs on narya", "resources/notes", None, None, Some("shesta")).unwrap();
        let hit = m.recall("bridge narya", 5).unwrap().into_iter().find(|e| e.uri == uri).expect("recalled");
        assert!(hit.tags.contains(&"author:shesta".to_string()), "free-form save stamps the author");

        // agent-less write: no author tag, exactly as before
        let uri = m.add_reminder("check backups", "verify k10 export", "agent/reminders", None).unwrap();
        let hit = m.reminders(5).unwrap().into_iter().find(|e| e.uri == uri).expect("listed");
        assert!(!hit.tags.iter().any(|t| t.starts_with("author:")), "agent-less write stays unstamped");
    }

    #[test]
    fn persona_for_serves_own_identity_plus_shared_governance() {
        let m = LocalMemory::for_test(tmp_store());
        m.import_record(Kind::Persona, "shared/governance", "House Rules", "shared boundaries").unwrap();
        m.import_record(Kind::Persona, "agents/izu/persona", "Izu Persona", "I am Izu").unwrap();
        m.import_record(Kind::Persona, "agents/shesta/persona", "Shesta Persona", "I am Shesta").unwrap();
        m.import_record(Kind::Protocol, "agent/protocol", "Behavioral Discipline", "recall before reasoning").unwrap();

        let izu: Vec<String> = m.persona_for(Some("izu")).unwrap().into_iter().map(|e| e.title).collect();
        assert!(izu.contains(&"Izu Persona".to_string()), "own persona served");
        assert!(izu.contains(&"House Rules".to_string()), "shared governance served");
        assert!(izu.contains(&"Behavioral Discipline".to_string()), "protocols are shared");
        assert!(!izu.contains(&"Shesta Persona".to_string()), "another agent's persona never leaks");

        // agent-less: the legacy full set (backward compatibility until migration)
        let all: Vec<String> = m.persona().unwrap().into_iter().map(|e| e.title).collect();
        assert!(all.contains(&"Izu Persona".to_string()) && all.contains(&"Shesta Persona".to_string()));
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
        let narya = m.import_record(Kind::Site, "resources/entities", "narya",
            &entity_body(Kind::Site, "narya", &[], "Homelab node")).unwrap();
        let izuh = m.import_record(Kind::Site, "resources/entities", "Izuhomeland (Windows)",
            &entity_body(Kind::Site, "Izuhomeland (Windows)", &[], "Workstation")).unwrap();
        let hit = m.remember("Rebooted narya after the NFS mount hung", "resources/notes", None, None, None).unwrap();
        m.remember("Vectors and naryad are not mentions of the node", "resources/notes", None, None, None).unwrap();
        let cased = m.remember("NARYA in caps must not match the conservative pass", "resources/notes", None, None, None).unwrap();
        let alias = m.remember("Provisioned Izuhomeland with Claude Code", "resources/notes", None, None, None).unwrap();
        // dry run counts but writes nothing
        let (found_dry, added_dry) = m.reindex_mentions(true).unwrap();
        assert!(added_dry >= 2, "dry run sees the candidates");
        assert!(m.edges_of(&hit).unwrap().is_empty(), "dry run must not write edges");
        // real pass links exactly the word-boundary, exact-case mentions (alias via parenthetical strip)
        let (found, added) = m.reindex_mentions(false).unwrap();
        assert_eq!((found, added), (found_dry, added_dry), "dry run and real pass agree");
        assert!(m.edges_of(&hit).unwrap().iter().any(|e| e.to_uri == narya && e.rel == "mentions"));
        assert!(m.edges_of(&alias).unwrap().iter().any(|e| e.to_uri == izuh), "parenthetical-stripped alias resolves");
        assert!(m.edges_of(&cased).unwrap().is_empty(), "exact case only: NARYA does not match narya");
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
            m.import_record(Kind::Persona, "agents/izu", "poison", "planted persona").is_err(),
            "scoped identities cannot write into the agents/ tree"
        );
    }

    #[test]
    fn agent_view_guards_the_agents_tree_on_every_surface() {
        // Audit High #1: an agent identity must not read, write, or retract another agent's
        // agents/<other>/... records through ANY surface - not just the persona route.
        let mut m = LocalMemory::for_test(tmp_store());
        let shesta_persona = m
            .import_record(Kind::Persona, "agents/shesta", "Shesta persona", "I am Shesta the document specialist")
            .unwrap();
        let shared = m.remember("Shared brain fact for everyone", "resources/notes", None, None, None).unwrap();

        m.set_agent_view(Some("izu".into()));
        // reads: foreign persona invisible everywhere
        assert!(m.recall("I am Shesta specialist", 5).unwrap().iter().all(|e| e.uri != shesta_persona));
        assert!(m.recent(50).unwrap().iter().all(|e| e.uri != shesta_persona));
        assert!(m.history(&shesta_persona, 10).unwrap().is_empty(), "history gated too");
        assert!(m.store.get(&shesta_persona).unwrap().is_none(), "get() reads it as absent");
        // shared pool stays fully visible (the shared brain is the point)
        assert!(m.recent(50).unwrap().iter().any(|e| e.uri == shared));
        // writes into the foreign tree are rejected; own tree + shared are fine
        assert!(m.import_record(Kind::Persona, "agents/shesta", "Fake", "overwrite attempt").is_err());
        assert!(m.import_record(Kind::Persona, "agents/izu", "Izu persona", "own tree ok").is_ok());
        assert!(m.remember("shared write ok", "resources/notes", None, None, None).is_ok());
        // retractions of the foreign record are rejected (reads as not-found)
        assert!(m.forget(&shesta_persona).is_err());
        assert!(m.invalidate(&shesta_persona, crate::entry::now_ms() + 10).is_err());
        // agent-less view: everything back to legacy behavior
        m.set_agent_view(None);
        assert!(m.store.get(&shesta_persona).unwrap().is_some());
    }

    #[test]
    fn agent_tree_guard_covers_edges_of_and_case_variants() {
        let mut m = LocalMemory::for_test(tmp_store());
        let shesta = m
            .import_record(Kind::Persona, "agents/shesta", "Shesta persona", "I am Shesta")
            .unwrap();
        let shared = m.remember("Shared record with an edge", "resources/notes", None, None, None).unwrap();
        m.link(&shared, &shesta, "about").unwrap();

        m.set_agent_view(Some("izu".into()));
        // 2nd-opinion follow-up: /edges must not enumerate a foreign persona tree - neither
        // by querying the foreign uri directly (predictable path) nor via a shared record's
        // edge list leaking the foreign endpoint's slug.
        assert!(m.edges_of(&shesta).unwrap().is_empty(), "probing the foreign uri yields nothing");
        assert!(
            m.edges_of(&shared).unwrap().iter().all(|e| e.from_uri != shesta && e.to_uri != shesta),
            "a shared record's edges hide invisible endpoints"
        );
        // case-variant spelling is the SAME protected tree, not shared pool
        assert!(
            m.import_record(Kind::Persona, "Agents/Shesta", "sneaky", "case bypass attempt").is_err(),
            "Agents/Shesta writes are guarded like agents/shesta"
        );
        assert!(m.import_record(Kind::Persona, "Agents/IZU", "own tree, odd case", "fine").is_ok());
        m.set_agent_view(None);
        assert_eq!(m.edges_of(&shesta).unwrap().len(), 1, "unrestricted view unchanged");
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
        let lenovo = m
            .import_record(Kind::Org, "resources/entities", "Lenovo",
                &entity_body(Kind::Org, "Lenovo", &[("role".into(), "principal".into()), ("sector".into(), "private".into())], ""))
            .unwrap();
        let proj = m
            .import_record(Kind::Engagement, "resources/entities", "MyGovUC",
                &entity_body(Kind::Engagement, "MyGovUC", &[("stage".into(), "BAU".into())], ""))
            .unwrap();
        let sr630 = m
            .import_record(Kind::Product, "resources/entities", "Lenovo SR630", &entity_body(Kind::Product, "Lenovo SR630", &[], ""))
            .unwrap();
        m.link(&sr630, &lenovo, "made-by").unwrap();
        m.link(&proj, &sr630, "uses").unwrap();
        // the engagement reaches the product at 1 hop and the principal at 2 hops
        let n2 = m.neighbors(&[proj.clone()], 2, 10).unwrap();
        assert!(n2.contains(&sr630), "engagement -> product");
        assert!(n2.contains(&lenovo), "engagement -> product -> principal");
        // the entity kind survives recall
        let hits = m.recall("Lenovo SR630", 5).unwrap();
        assert!(hits.iter().any(|e| e.kind == Kind::Product && e.title == "Lenovo SR630"));
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
    static RECALL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn floor_disabled_matches_prefloor_recall() {
        // The kill-switch guarantee: DM_RECALL_FLOOR=0 reproduces pre-floor recall exactly (the
        // disabled keyword path is plain store.recall). This is what rollback depends on.
        let _g = RECALL_ENV_LOCK.lock().unwrap();
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
}

/// Step B: the recall-floor CALIBRATION harness. Dev-only and feature-gated to `candle` (the
/// production bge-small embedder), so it NEVER compiles into the release binary. It stands up a
/// real-embeddings store in an isolated temp dir (never the live arif.db), seeds a labeled
/// synthetic corpus (topic clusters + vector-only paraphrases with zero keyword overlap +
/// hard-negatives that share a keyword but are off-topic + fillers), sweeps (abs_cosine, rel_ratio)
/// on TRAIN queries, and validates the chosen thresholds on a HELD-OUT split. Run it with:
///   cargo test --features candle floor_eval -- --nocapture
/// then read the printed RECOMMEND line and bake it into `RecallFloor::DEFAULTS`.
#[cfg(all(test, feature = "candle"))]
mod floor_eval {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
            ("C1", "Submitted the MyGovUC migration proposal for the government tender."),
            ("C2", "The MCMC sovereign cloud tender requires local data residency in Malaysia."),
            ("C3", "KHD is the hardware distributor feeding the government tender bid."),
            ("C4", "Tender compliance for the JPN project needed CIDB and MOF certificates."),
            ("C5", "The MyGovUC engagement reached BAU after the migration delivery."),
            ("C6", "Prepared the BOM and sizing for the Malaysia public-sector tender."),
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
            ("MyGovUC government tender submission in Malaysia",
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
        let _g = ENV_LOCK.lock().unwrap();
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
                    let vec = m.vindex.as_ref().unwrap().search(&qv, POOL).unwrap();
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
