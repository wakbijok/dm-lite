//! High-level memory API: the typed guided save tools (per-kind required-field
//! validation) + recall. This is daimon's distinctive layer over the engine.

use crate::config;
use crate::entry::{make_uri, now_ms, Edge, Entry, Kind};
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
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    // Keyword-only mode: no cosine to gate on, so the bm25 relative gate is the floor.
    if vec.is_empty() {
        let top_kw = kw.iter().map(|(_, s)| *s).fold(f64::NEG_INFINITY, f64::max);
        return kw
            .iter()
            .filter(|(_, s)| *s >= f.abs_keyword && (top_kw <= 0.0 || *s >= f.rel_ratio * top_kw))
            .map(|(u, _)| u.clone())
            .collect();
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
        self.save_valid(kind, namespace, title, body, importance, tags, None, None)
    }

    /// As `save`, but with a caller-supplied valid interval (the bitemporal application-time axis).
    /// `valid_from = None` means now; `valid_to = None` means open (still true). The store's put
    /// does the valid-time splitting against any existing segments of this entity.
    #[allow(clippy::too_many_arguments)]
    fn save_valid(&self, kind: Kind, namespace: &str, title: &str, body: String, importance: i64, tags: Vec<String>, valid_from: Option<i64>, valid_to: Option<i64>) -> Result<String> {
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

    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>) -> Result<String> {
        require(text, "text")?;
        let title = first_line(text);
        self.save_valid(Kind::Memory, namespace, &title, text.to_string(), 50, vec![], valid_from, valid_to)
    }

    /// Application-time invalidation: this entity's fact is no longer true from `valid_to_ms` on.
    pub fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        self.store.invalidate(uri, valid_to_ms)
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
            let keep = floor_survivors(&kw, &[], &floor);
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
        let keep = if floor.enabled { Some(floor_survivors(&kw, &vec, &floor)) } else { None };
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

    // --- graph layer ---

    pub fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()> {
        self.store.link(from_uri, to_uri, rel)
    }
    pub fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        self.store.unlink(from_uri, to_uri, rel)
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
        let seeds = self.recall(query, limit)?;
        if depth == 0 {
            return Ok(seeds);
        }
        let seed_uris: Vec<String> = seeds.iter().map(|e| e.uri.clone()).collect();
        let mut seen: std::collections::HashSet<String> = seed_uris.iter().cloned().collect();
        let mut out = seeds;
        for uri in self.store.neighbors(&seed_uris, depth, limit)? {
            if seen.insert(uri.clone()) {
                if let Some(e) = self.store.get(&uri)? {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Rebuild edges from the `[[name]]` references in every current record's body. Batch, not
    /// on-save, so writes stay fast at scale: build a slug->uri map once (the slug is the uri's
    /// last segment), then resolve each `[[name]]` against it in memory. Idempotent. Returns the
    /// count of `[[name]]` references that resolved to a record and were linked.
    pub fn reindex_links(&self) -> Result<usize> {
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
        Ok(linked)
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
    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>) -> Result<String> {
        match self {
            Memory::Local(l) => l.remember(text, namespace, valid_from, valid_to),
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
    pub fn reindex_links(&self) -> Result<usize> {
        match self {
            Memory::Local(l) => l.reindex_links(),
            #[cfg(feature = "client")]
            Memory::Remote(r) => r.reindex_links(),
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
    fn remember_valid_and_invalidate_wire_through_the_api() {
        let m = LocalMemory::for_test(tmp_store());
        let uri = m.remember("status is green", "resources/notes", Some(100), None).unwrap();
        assert_eq!(m.invalidate(&uri, 300).unwrap(), 1, "one segment invalidated");
        assert!(m.recall("green", 5).unwrap().is_empty(), "no longer valid now");
        let past = m.recall_as_of("green", 5, now_ms(), 200).unwrap();
        assert!(past.iter().any(|e| e.body.contains("green")), "valid-as-of 200 still sees it");
    }

    #[test]
    fn reindex_links_resolves_wikilinks_and_recall_expands() {
        let m = LocalMemory::for_test(tmp_store());
        m.remember("Beta the target", "resources/notes", None, None).unwrap();
        m.remember("Alpha refers to [[Beta the target]] for context", "resources/notes", None, None).unwrap();
        let n = m.reindex_links().unwrap();
        assert!(n >= 1, "the [[Beta the target]] reference should resolve and link");
        // a query that only hits alpha still pulls beta in, via the edge
        let hits = m.recall_expanded("Alpha refers context", 3, 1).unwrap();
        assert!(hits.iter().any(|e| e.body.contains("Beta the target")), "neighbor pulled in via the graph");
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
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45));
        assert!(keep.is_empty(), "all cosines < 0.30 must inject nothing, got {keep:?}");
    }

    #[test]
    fn floor_adaptive_keeps_strong_drops_weak_tail() {
        // two strong hits then a steep drop-off: the relative ratio drops the tail, the absolute
        // floor is cleared by the strong ones. A 2-relevant query injects ~2, not the whole pool.
        let vec = vec![v("s1", 0.82), v("s2", 0.78), v("w1", 0.33), v("w2", 0.31)];
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45));
        assert!(keep.contains("s1") && keep.contains("s2"), "strong hits kept");
        assert!(!keep.contains("w1") && !keep.contains("w2"), "weak tail dropped by ratio: {keep:?}");
    }

    #[test]
    fn floor_negative_top_cosine_does_not_admit_worse() {
        // all-negative cosine (off-topic): the relative clause is skipped (top <= 0) so it can't
        // invert into admitting worse hits; the absolute gate empties the channel.
        let vec = vec![v("a", -0.10), v("b", -0.20), v("c", -0.05)];
        let keep = floor_survivors(&[], &vec, &floor(0.30, 0.0, 0.45));
        assert!(keep.is_empty(), "negative cosines must not survive, got {keep:?}");
    }

    #[test]
    fn floor_cosine_is_the_floor_in_hybrid_keyword_cannot_bypass() {
        // the leak fix: a junk record that shares a common WORD with the query (strong bm25) but is
        // semantically distant (low cosine) must NOT survive in hybrid - cosine is the floor, the
        // keyword channel cannot bypass it. The genuinely relevant hit (high cosine) survives.
        let kw = vec![kwh("kw_overlap_junk", 9.0), kwh("relevant", 3.0)];
        let vec = vec![v("relevant", 0.82), v("kw_overlap_junk", 0.18)];
        let keep = floor_survivors(&kw, &vec, &floor(0.30, 0.0, 0.45));
        assert!(keep.contains("relevant"), "semantically-relevant hit survives");
        assert!(!keep.contains("kw_overlap_junk"), "shared-word junk with low cosine must be dropped: {keep:?}");
    }

    #[test]
    fn floor_keyword_only_mode_uses_bm25_relative_gate() {
        // no vector channel (keyword-only build / search failed): the bm25 relative gate is the
        // floor - the top match and anything within the ratio survive, the weak tail is dropped.
        let kw = vec![kwh("strong", 8.0), kwh("mid", 4.0), kwh("weak", 1.0)];
        let keep = floor_survivors(&kw, &[], &floor(0.30, 0.0, 0.45)); // 0.45*8 = 3.6
        assert!(keep.contains("strong") && keep.contains("mid"), "top + within-ratio kept");
        assert!(!keep.contains("weak"), "weak tail (1.0 < 3.6) dropped: {keep:?}");
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
        m.remember("alpha bravo charlie delta", "resources/notes", None, None).unwrap();
        m.remember("alpha only here", "resources/notes", None, None).unwrap();
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
        assert_eq!(floor_survivors(&kw, &[], &floor(0.30, 0.0, 0.45)).len(), 2, "keyword-only mode");
        // hybrid: a low-cosine vector pool would gate everything, but INFINITY recent rows still pass
        let vec = vec![v("x", 0.05)];
        let keep = floor_survivors(&kw, &vec, &floor(0.30, 0.0, 0.45));
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
        let keep = floor_survivors(&q.kw, &q.vec, f);
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
            uri_of.insert(label, m.remember(text, "resources/eval", None, None).unwrap());
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
