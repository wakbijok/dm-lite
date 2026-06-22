//! High-level memory API: the typed guided save tools (per-kind required-field
//! validation) + recall. This is daimon's distinctive layer over the engine.

use crate::config;
use crate::entry::{make_uri, now_ms, Entry, Kind};
use crate::sqlite::SqliteStore;
use crate::store::MemoryStore;
use anyhow::{anyhow, Result};

/// The local (embedded) memory engine: SQLite store + optional zvec vector index.
pub struct LocalMemory {
    store: SqliteStore,
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

impl LocalMemory {
    /// Open the embedded-mode tenant ($DM_TENANT, else "default").
    pub fn open() -> Result<Self> {
        Self::open_tenant(&config::tenant())
    }

    /// Open a specific tenant's store explicitly. Server mode uses this per request so it
    /// never mutates the process-global $DM_TENANT (which would race under concurrency).
    pub fn open_tenant(tenant: &str) -> Result<Self> {
        let path = config::db_path(tenant)?;
        let store = SqliteStore::open(&path)?;
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
            return Ok(Self { store, vindex, embedder: make_embedder() });
        }
        #[cfg(not(feature = "zvec"))]
        Ok(Self { store })
    }

    fn save(&self, kind: Kind, namespace: &str, title: &str, body: String, importance: i64, tags: Vec<String>) -> Result<String> {
        let uri = make_uri(namespace, kind, title);
        let e = Entry::new_now(
            uri.clone(),
            kind,
            namespace.to_string(),
            title.to_string(),
            body,
            tags,
            importance,
            uri.clone(),
        );
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
            let v = self.embedder.embed(&e.body);
            if let Err(err) = vindex.upsert(&e.uri, &v) {
                eprintln!("dmem: vector index upsert failed for {} ({err:#}); keyword recall unaffected", e.uri);
            }
        }
        Ok(())
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

    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(decision, "decision")?;
        let body = format!(
            "# {}\n\n**Context:** {}\n\n**Decision:** {}\n\n**Rationale:** {}\n",
            title, context, decision, rationale
        );
        self.save(Kind::Decision, namespace, title, body, 70, vec!["decision".into()])
    }

    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(lesson, "lesson")?;
        let body = format!("# {}\n\n**Lesson:** {}\n", title, lesson);
        self.save(Kind::AgentLesson, namespace, title, body, 60, vec!["agent_lesson".into()])
    }

    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(impact, "impact")?;
        let body = format!(
            "# {}\n\n**Impact:** {}\n\n**Resolution:** {}\n",
            title, impact, resolution
        );
        self.save(Kind::IncidentSummary, namespace, title, body, 65, vec!["incident_summary".into()])
    }

    pub fn remember(&self, text: &str, namespace: &str) -> Result<String> {
        require(text, "text")?;
        let title = first_line(text);
        self.save(Kind::Memory, namespace, &title, text.to_string(), 50, vec![])
    }

    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(text, "text")?;
        let body = format!("# {}\n\n**Reminder:** {}\n", title, text);
        self.save(Kind::Reminder, namespace, title, body, 55, vec!["reminder".into()])
    }

    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(steps, "steps")?;
        let body = format!("# {}\n\n**Runbook:** {}\n", title, steps);
        self.save(Kind::Runbook, namespace, title, body, 60, vec!["runbook".into()])
    }

    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(rule, "rule")?;
        let body = format!("# {}\n\n**Convention:** {}\n", title, rule);
        self.save(Kind::ProjectConvention, namespace, title, body, 65, vec!["project_convention".into()])
    }

    /// Import a record of any kind from a template/file (the write path for persona/protocol).
    pub fn import_record(&self, kind: Kind, namespace: &str, title: &str, body: &str) -> Result<String> {
        require(title, "title")?;
        self.save(kind, namespace, title, body.to_string(), crate::entry::default_importance(kind), vec![])
    }

    /// Count of live records per kind (for `dm status`).
    pub fn counts(&self) -> Result<Vec<(String, usize)>> {
        let all = self.store.recent(1_000_000)?;
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
        // keyword hits), then apply the modest runtime-signal rescoring.
        let pool = (limit * 2).max(10);
        let hits = self.store.recall(query, pool)?;
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
        let kw: Vec<String> = self.store.recall(query, pool)?.into_iter().map(|e| e.uri).collect();
        let qv = self.embedder.embed(query);
        let vec: Vec<String> = match vindex.search(&qv, pool) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("dmem: vector search failed ({e:#}); using keyword results only");
                Vec::new()
            }
        };
        let k = 60.0_f64;
        let mut score: HashMap<String, f64> = HashMap::new();
        for (rank, uri) in kw.iter().enumerate() {
            *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, uri) in vec.iter().enumerate() {
            *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        // Hydrate, then apply the modest runtime-signal multiplier AFTER RRF: a bounded
        // (<=1.25x) nudge that reorders near-equal scores without overturning a clear gap.
        let now = now_ms();
        let uris: Vec<String> = score.keys().cloned().collect();
        let sigs = self.store.read_signals(&uris).unwrap_or_default();
        let mut scored: Vec<(Entry, f64)> = Vec::new();
        for (uri, rrf) in score {
            if let Some(e) = self.store.get(&uri)? {
                let (ac, la) = sigs.get(&uri).copied().unwrap_or((0, 0));
                let s = rrf * signal_boost(e.importance, ac, la, now);
                scored.push((e, s));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let out: Vec<Entry> = scored.into_iter().take(limit).map(|(e, _)| e).collect();
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

    /// Best-effort: bump the access signal for each recalled record. Never fails recall.
    fn bump_recalled(&self, entries: &[Entry]) {
        let now = now_ms();
        for e in entries {
            let _ = self.store.bump_signal(&e.uri, now);
        }
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

    /// Persona + protocol records (the boot layer), most important first.
    pub fn persona(&self) -> Result<Vec<Entry>> {
        let mut out = self.store.by_kind("persona", 5)?;
        out.extend(self.store.by_kind("protocol", 5)?);
        Ok(out)
    }

    /// Open reminders (kind=reminder), most important/recent first. The session-start greet
    /// pulls a few of these; the full backlog is on-demand recall.
    pub fn reminders(&self, limit: usize) -> Result<Vec<Entry>> {
        self.store.by_kind("reminder", limit)
    }

    /// System-time of the most recent save (for the save-discipline nudge cadence).
    pub fn latest_save_ms(&self) -> Result<Option<i64>> {
        self.store.latest_save_ms()
    }

    /// Construct a LocalMemory directly over a store (tests only; bypasses config). Uses the
    /// cheap HashEmbedder rather than `make_embedder` so tests never load a real model (no
    /// network, fast, deterministic); `vindex: None` keeps recall on the keyword path.
    #[cfg(test)]
    pub(crate) fn for_test(store: SqliteStore) -> Self {
        #[cfg(feature = "zvec")]
        {
            Self { store, vindex: None, embedder: std::sync::Arc::new(crate::embedder::HashEmbedder::new()) }
        }
        #[cfg(not(feature = "zvec"))]
        {
            Self { store }
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
    pub fn remember(&self, text: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.remember(text, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.remember(text, namespace),
        }
    }
    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_decision(title, context, decision, rationale, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_decision(title, context, decision, rationale, namespace),
        }
    }
    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_lesson(title, lesson, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_lesson(title, lesson, namespace),
        }
    }
    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_incident(title, impact, resolution, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_incident(title, impact, resolution, namespace),
        }
    }
    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.add_reminder(title, text, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.add_reminder(title, text, namespace),
        }
    }
    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_runbook(title, steps, namespace),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.log_runbook(title, steps, namespace),
        }
    }
    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str) -> Result<String> {
        match self {
            Memory::Local(l) => l.log_convention(title, rule, namespace),
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
}
