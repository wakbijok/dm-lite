//! The storage seam. v2's locked engine is LanceDB (GA vector + hybrid); M0 ships the
//! reliable SQLite impl behind this trait so the binary works + is testable today, and
//! LanceDB drops in as another impl with zero change to the model or the callers.

use crate::entry::Entry;
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
}
