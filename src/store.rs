//! The storage seam. v2's locked engine is LanceDB (GA vector + hybrid); M0 ships the
//! reliable SQLite impl behind this trait so the binary works + is testable today, and
//! LanceDB drops in as another impl with zero change to the model or the callers.

use crate::entry::{Edge, Entry};
use anyhow::Result;

pub trait MemoryStore {
    /// Upsert by dedup_key: close any prior live record with the same dedup_key
    /// (close-not-delete), then insert the new one.
    fn put(&self, e: &Entry) -> Result<()>;

    /// Hybrid recall. M0 is keyword-only (FTS); dense vector + RRF layer in behind the
    /// same signature when the embedder is present. Returns live records, best first.
    fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>>;

    /// Recent high-importance live records (empty-query recall, for SessionStart).
    fn recent(&self, limit: usize) -> Result<Vec<Entry>>;

    /// All live records of a kind (for persona/protocol injection).
    fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Entry>>;

    /// Recall as the store existed AS OF system-time `as_of_ms`, for facts VALID AT
    /// `valid_ms`. Keyword-only (the FTS + vector indexes hold only the current version);
    /// a linear scan over history reconstructs the past slice. Best first.
    fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>>;

    /// All recorded versions of a uri, newest system-time first (full append-only lineage).
    fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>>;

    /// Retract a uri: close its current version(s) in system time so it drops out of recall,
    /// keeping the lineage (append-only, never hard-deleted). Returns how many were closed.
    fn forget(&self, uri: &str) -> Result<usize>;

    /// System-time of the most recent write of ANY version (`MAX(system_from_ms)`), or None for
    /// an empty store. This is "when did I last save", used by the save-discipline nudge cadence;
    /// unlike `recent`, it is ordered by time, not importance.
    fn latest_save_ms(&self) -> Result<Option<i64>>;

    /// Application-time invalidation: mark this uri's fact as no longer true from `valid_to_ms`
    /// onward, keeping the historical `[valid_from, valid_to_ms)` slice queryable via as-of. This
    /// is a VALID-time end, distinct from `forget` (which retracts from current belief in SYSTEM
    /// time, as if we never should have recorded it). Returns how many segments were affected.
    fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize>;

    // --- graph layer (edges between records) ---

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

    /// All edges (capped), for the graph viewer.
    fn all_edges(&self, limit: usize) -> Result<Vec<Edge>>;

    /// Resolve a `[[name]]` slug to a current record's uri (the slug is the uri's last segment),
    /// most-important/recent first. None if nothing matches. Used to turn body `[[links]]` into
    /// real edges.
    fn resolve_slug(&self, slug: &str) -> Result<Option<String>>;
}
