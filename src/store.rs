//! The storage seam. v2's locked engine is LanceDB (GA vector + hybrid); M0 ships the
//! reliable SQLite impl behind this trait so the binary works + is testable today, and
//! LanceDB drops in as another impl with zero change to the model or the callers.

use crate::entry::{Edge, Entry};
use anyhow::Result;

pub trait MemoryStore {
    /// Upsert by dedup_key: close any prior live record with the same dedup_key
    /// (close-not-delete), then insert the new one.
    fn put(&self, e: &Entry) -> Result<()>;

    /// Keyword relevance MAGNITUDE per hit (higher = better), best first. The SQLite impl returns
    /// `-bm25` (SQLite's bm25 rank is negative, more-negative = better, so the negation makes larger
    /// = better). This is the recall primitive: `recall` is derived from it by dropping the score.
    /// It feeds the relevance floor's keyword gate; the empty/short-query fallback (no keyword
    /// magnitude) returns `f64::INFINITY` so those boot/recent rows are never gated out. An impl
    /// that cannot score keywords should return a uniform `f64::INFINITY` (its keyword gate becomes
    /// a no-op). M0 is keyword-only (FTS); the dense vector + RRF layer fuses in above this, in the
    /// caller, when the embedder is present.
    fn recall_scored(&self, query: &str, limit: usize) -> Result<Vec<(Entry, f64)>>;

    /// Live records best first (scores dropped). Derived from `recall_scored` so there is a single
    /// query path; callers that do not need magnitudes use this.
    fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        Ok(self.recall_scored(query, limit)?.into_iter().map(|(e, _)| e).collect())
    }

    /// Recent high-importance live records (empty-query recall, for SessionStart).
    fn recent(&self, limit: usize) -> Result<Vec<Entry>>;

    /// All live records of a kind (for persona/protocol injection).
    fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Entry>>;

    /// Live records of a kind VISIBLE TO an agent identity (the per-agent governance query).
    /// `None` = no agent identity: every record, exactly `by_kind` (legacy tokens, embedded
    /// mode). `Some(a)` = records whose namespace lies outside the `agents/` tree (shared
    /// governance goes to everyone) plus those under `agents/<a>` (that agent's own); another
    /// agent's `agents/<b>/...` records are excluded. Filtering happens at the query so the
    /// LIMIT applies to the visible set, not before it.
    fn by_kind_for_agent(&self, kind: &str, agent: Option<&str>, limit: usize) -> Result<Vec<Entry>>;

    /// Recall as the store existed AS OF system-time `as_of_ms`, for facts VALID AT
    /// `valid_ms`. Keyword-only (the FTS + vector indexes hold only the current version);
    /// a linear scan over history reconstructs the past slice. Best first.
    fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>>;

    /// All recorded versions of a uri, newest system-time first (full append-only lineage).
    fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>>;

    /// Retract a uri: close its current version(s) in system time so it drops out of recall,
    /// keeping the lineage (append-only, never hard-deleted). Returns how many were closed.
    /// Every scope's live rows (the full-tenant writer's view): `forget_in(uri, None)`.
    fn forget(&self, uri: &str) -> Result<usize> {
        self.forget_in(uri, None)
    }

    /// Scope-exact retraction. `None` = every scope (full-tenant writer); `Some(ws)` closes
    /// ONLY the live rows of `uri` whose scope is `ws` - another scope holding the same uri is
    /// another audience's record and is never touched. The return counts the rows closed in
    /// that selection only. The graph cascade (the uri's edges) runs only when, after the
    /// close, no live row of the uri remains in ANY scope.
    fn forget_in(&self, uri: &str, scope: Option<&str>) -> Result<usize>;

    /// System-time of the most recent write of ANY version (`MAX(system_from_ms)`), or None for
    /// an empty store. This is "when did I last save", used by the save-discipline nudge cadence;
    /// unlike `recent`, it is ordered by time, not importance.
    fn latest_save_ms(&self) -> Result<Option<i64>>;

    /// Application-time invalidation: mark this uri's fact as no longer true from `valid_to_ms`
    /// onward, keeping the historical `[valid_from, valid_to_ms)` slice queryable via as-of. This
    /// is a VALID-time end, distinct from `forget` (which retracts from current belief in SYSTEM
    /// time, as if we never should have recorded it). Returns how many segments were affected.
    /// Every scope's live rows (the full-tenant writer's view): `invalidate_in(.., None)`.
    fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        self.invalidate_in(uri, valid_to_ms, None)
    }

    /// Scope-exact invalidation, with the same selection rule as `forget_in`: `None` = every
    /// scope, `Some(ws)` = only the live segments in `ws`. Returns the segments affected in
    /// that selection only. A cut at/before a segment's start drops it with no remainder; when
    /// the call leaves the uri with no live row in any scope, its edges cascade exactly as
    /// after `forget_in`, in the same transaction.
    fn invalidate_in(&self, uri: &str, valid_to_ms: i64, scope: Option<&str>) -> Result<usize>;

    // --- graph layer (edges between records) ---
    // A uri's edges go with its LAST live row: `forget_in` and `invalidate_in` cascade them
    // once no live row of the uri remains in any scope (a scoped retraction that leaves another
    // scope's row live keeps them, and so does an invalidation that leaves a live remainder -
    // that node still exists). Edges can still outlive an endpoint (links made to
    // a never-existed uri, rot from before the cascade existed); reads handle this gracefully
    // (recall_expanded hydrates only current records via get(); the viewer drops edges whose
    // endpoints are absent) and `prune_dangling_edges` clears it. Remove an edge explicitly
    // with unlink.

    /// Add a typed directed edge `from_uri -[rel]-> to_uri`. Idempotent (a duplicate edge is a
    /// no-op). Edges are curated relations, not bitemporal facts: re-deriving them is safe.
    fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()>;

    /// Remove a specific edge. Returns how many rows were deleted (0 or 1).
    fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize>;

    /// Every edge touching `uri`, in either direction (its immediate connections).
    fn edges_of(&self, uri: &str) -> Result<Vec<Edge>>;

    /// Bounded-hop traversal: the set of record uris reachable from any of `seeds` within `depth`
    /// hops (following edges in either direction), excluding the seeds themselves, capped at
    /// `limit`. This is the recall-expansion primitive: pull a seed's neighborhood, not the world.
    fn neighbors(&self, seeds: &[String], depth: usize, limit: usize) -> Result<Vec<String>>;

    /// Scored bounded-hop traversal: like `neighbors`, but each seed carries a weight and each
    /// reached node is scored `seed_weight * decay^hop` along its best-scoring arrival path, so
    /// the caller can fill rider slots by relevance instead of BFS arrival order. Level-order
    /// walk with per-node best-score: a node's `hop` is fixed by its first (shallowest) arrival;
    /// its score/via/rel upgrade if a later arrival scores higher; nodes are expanded once (no
    /// re-expansion on upgrade - with a uniform per-hop decay the improvement downstream is
    /// second-order, and the walk stays linear). Seeds are frozen: never scored, never emitted.
    /// `limit` caps the number of distinct reached nodes. Results come back best score first
    /// (ties: shallower hop, then uri, so the order is deterministic).
    fn neighbors_scored(
        &self,
        seeds: &[(String, f64)],
        depth: usize,
        limit: usize,
        decay: f64,
    ) -> Result<Vec<NeighborHit>> {
        use std::collections::{HashMap, HashSet};
        if seeds.is_empty() || depth == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        let seed_set: HashSet<&str> = seeds.iter().map(|(u, _)| u.as_str()).collect();
        let mut best: HashMap<String, NeighborHit> = HashMap::new();
        let mut frontier: Vec<(String, f64)> = seeds.to_vec();
        for hop in 1..=(depth as u32) {
            let mut next: Vec<(String, f64)> = Vec::new();
            for (u, w) in &frontier {
                for edge in self.edges_of(u)? {
                    let other = if edge.from_uri == *u { &edge.to_uri } else { &edge.from_uri };
                    if other == u || seed_set.contains(other.as_str()) {
                        continue;
                    }
                    // Scope gate at traversal, not hydration: an out-of-scope node is not on
                    // the graph for this reader - it neither rides nor bridges.
                    if !self.scope_visible(other)? {
                        continue;
                    }
                    let score = w * decay;
                    match best.get_mut(other) {
                        Some(hit) => {
                            if score > hit.score {
                                hit.score = score;
                                hit.via = u.clone();
                                hit.rel = edge.rel.clone();
                            }
                        }
                        None => {
                            if best.len() >= limit {
                                continue;
                            }
                            best.insert(
                                other.clone(),
                                NeighborHit { uri: other.clone(), hop, via: u.clone(), rel: edge.rel.clone(), score },
                            );
                            next.push((other.clone(), score));
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let mut out: Vec<NeighborHit> = best.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.hop.cmp(&b.hop))
                .then(a.uri.cmp(&b.uri))
        });
        Ok(out)
    }

    /// All edges (capped), for the graph viewer.
    fn all_edges(&self, limit: usize) -> Result<Vec<Edge>>;

    /// May the current reader see this uri at all? (Scope primitive.) This is the
    /// TRAVERSAL-level gate: graph expansion must not walk THROUGH an invisible node -
    /// rider `via` provenance would leak its title otherwise. A uri with no live row is judged
    /// by its newest row ever (a retracted record's uri stays as hidden as the record was);
    /// only a uri with no row at all is visible. Default: everything visible (stores without
    /// scope support behave exactly as before).
    fn scope_visible(&self, uri: &str) -> Result<bool> {
        let _ = uri;
        Ok(true)
    }

    /// Graph hygiene: delete every edge with at least one dead endpoint. Returns how many
    /// edges were pruned. An endpoint is DEAD when its uri has no system-open version at all
    /// (forgotten, or never existed); an INVALIDATED record (valid-time closed, system-time
    /// open) is still a live node of the belief history and keeps its edges. `forget_in` and
    /// `invalidate_in` cascade a uri's edges when its last live row goes; this is the batch
    /// pass that clears rot accumulated before that, plus manual links to targets that never
    /// existed. Recall
    /// stays tolerant of dangling edges regardless (dead riders are skipped without
    /// consuming the cap) - pruning stops them from also bridging BFS expansion.
    fn prune_dangling_edges(&self) -> Result<usize>;
}

/// A graph-expansion hit from `neighbors_scored`: a non-seed record reached by walking edges out
/// from the recall seeds, with enough provenance to rank and explain it - `hop` (1 = adjacent to
/// a seed), `via` (the uri it was reached from on its best-scoring path), `rel` (that edge's
/// relation), and `score` (`seed_weight * decay^hop`, best across arrival paths).
#[derive(Debug, Clone, PartialEq)]
pub struct NeighborHit {
    pub uri: String,
    pub hop: u32,
    pub via: String,
    pub rel: String,
    pub score: f64,
}
