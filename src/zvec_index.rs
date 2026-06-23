//! zvec-backed vector index (Alibaba zvec, in-process). Wak's chosen vector substrate
//! (decision 93dc17fd, overriding the LanceDB recommendation). Stores uri -> embedding;
//! search returns the nearest uris. Behind the `zvec` feature; SQLite stays the canonical
//! record + keyword store, this is the dense-vector half fused via RRF in recall.

use crate::embedder::DIM;
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::Once;
use zvec::*;

static INIT: Once = Once::new();

fn ze<E: std::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow!("zvec: {:?}", e)
}

/// zvec's primary key is capped at 64 bytes and rejects `:` / `/` (it reports both as
/// "contains invalid characters"), so a daimon:// URI can't be the PK directly. We derive a
/// short, fixed-length, charset-safe PK by hashing the URI; the real URI is stored in the
/// "uri" string field (string field values are unrestricted) and read back on search. The
/// hash is deterministic, so re-saving the same URI supersedes its prior vector.
fn pk_for(uri: &str) -> String {
    // 128-bit FNV-1a (two independent streams) -> 32 hex chars. Well under the 64-byte cap;
    // collision-free at our scale (tens-to-hundreds of records per tenant).
    fn fnv1a(seed: u64, s: &str) -> u64 {
        let mut h: u64 = seed;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
    let a = fnv1a(0xcbf29ce484222325, uri);
    let b = fnv1a(0x84222325cbf29ce4, uri);
    format!("{a:016x}{b:016x}")
}

pub struct ZvecIndex {
    collection: Collection,
}

impl ZvecIndex {
    /// Open the vector collection under `dir`, creating it the first time. zvec's
    /// `create_and_open` rejects an existing path, so we create-or-open explicitly.
    pub fn open(dir: &Path) -> Result<Self> {
        INIT.call_once(|| {
            let _ = initialize(None);
        });
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let path = dir.to_string_lossy().to_string();
        let collection = if dir.exists() {
            Collection::open(&path, None).map_err(ze)?
        } else {
            let schema = CollectionSchema::builder("entries")
                .add_field(FieldSchema::new("uri", DataType::String, false, 0).map_err(ze)?)
                .add_vector_field(
                    "embedding",
                    DataType::VectorFp32,
                    DIM as u32,
                    IndexParams::hnsw(MetricType::Cosine, 16, 200).map_err(ze)?,
                )
                .build()
                .map_err(ze)?;
            Collection::create_and_open(&path, &schema, None).map_err(ze)?
        };
        Ok(Self { collection })
    }

    /// Upsert (supersede) the vector for a uri. PK is the hashed uri; the real uri is a field.
    pub fn upsert(&self, uri: &str, vector: &[f32]) -> Result<()> {
        let pk = pk_for(uri);
        let _ = self.collection.delete(&[pk.as_str()]); // best-effort close prior
        let mut doc = Doc::new().map_err(ze)?;
        doc.set_pk(&pk);
        doc.add_string("uri", uri).map_err(ze)?;
        doc.add_vector_f32("embedding", vector).map_err(ze)?;
        self.collection.insert(&[&doc]).map_err(ze)?;
        Ok(())
    }

    /// Remove the vector for a uri (used by forget). Best-effort idempotent.
    pub fn remove(&self, uri: &str) -> Result<()> {
        self.collection.delete(&[pk_for(uri).as_str()]).map_err(ze)?;
        Ok(())
    }

    /// Nearest (uri, similarity) to the query vector, best first. zvec's `get_score()` for a
    /// MetricType::Cosine HNSW index returns a cosine DISTANCE (0.0 = identical, 1.0 = orthogonal,
    /// 2.0 = opposite; smaller = nearer), verified empirically. Our embeddings are L2-normalized,
    /// so we convert to a true cosine SIMILARITY = 1.0 - distance, giving [-1, 1] with HIGHER =
    /// more similar - the form the recall relevance floor (abs_cosine) thresholds on. Surfacing
    /// this magnitude (not just the uri) is what lets the floor drop semantically-distant hits.
    pub fn search(&self, vector: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        let q = SearchQuery::builder()
            .field_name("embedding")
            .vector(vector)
            .topk(k as i32)
            .output_fields(&["uri"])
            .build()
            .map_err(ze)?;
        let results = self.collection.query(&q).map_err(ze)?;
        let mut out = Vec::new();
        for r in &results {
            if let Ok(Some(uri)) = r.get_string("uri") {
                out.push((uri, 1.0 - r.get_score()));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_search_finds_nearest() {
        let dir = std::env::temp_dir().join(format!("zvectest-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut a = vec![0f32; DIM];
        a[0] = 1.0;
        let mut b = vec![0f32; DIM];
        b[100] = 1.0;
        idx.upsert("uri-a", &a).expect("upsert a");
        idx.upsert("uri-b", &b).expect("upsert b");
        let hits = idx.search(&a, 5).expect("search");
        assert!(
            hits.first().map(|(s, _)| s == "uri-a").unwrap_or(false),
            "nearest to a should be uri-a, got {:?}",
            hits
        );
        // search() returns cosine SIMILARITY (higher = nearer): self-match ~1.0, orthogonal ~0.0
        let by: std::collections::HashMap<_, _> = hits.iter().cloned().collect();
        assert!(by["uri-a"] > 0.9, "self-match similarity should be ~1.0, got {:?}", hits);
        assert!(
            by["uri-a"] > by.get("uri-b").copied().unwrap_or(f32::MIN),
            "aligned vector must outscore the orthogonal one, got {:?}",
            hits
        );
    }

    #[test]
    fn persists_and_searches_across_reopen() {
        // mirrors real dmem: one process writes, a later process reopens + searches
        let dir = std::env::temp_dir().join(format!("zvecreopen-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = vec![0f32; DIM];
        a[5] = 1.0;
        {
            let idx = ZvecIndex::open(&dir).expect("open(create)");
            idx.upsert("uri-a", &a).expect("upsert");
        } // drop / close
        let idx2 = ZvecIndex::open(&dir).expect("open(existing)");
        let hits = idx2.search(&a, 5).expect("search after reopen");
        assert!(
            hits.iter().any(|(u, _)| u == "uri-a"),
            "reopened index should still find uri-a, got {:?}",
            hits
        );
    }

    #[test]
    fn long_daimon_uri_roundtrips() {
        // the real failure: a daimon:// URI is >64 chars and has : and / — so it can't be the
        // PK (zvec caps PK at 64 bytes). Hashed PK + uri field must round-trip the full URI.
        let dir = std::env::temp_dir().join(format!("zveclonguri-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let uri = "daimon://resources/notes/memory/the-postgres-database-server-was-oom-killed-during-migration";
        assert!(uri.len() > 64, "fixture must exceed the 64-byte PK cap");
        let mut v = vec![0f32; DIM];
        v[7] = 1.0;
        idx.upsert(uri, &v).expect("upsert long daimon uri");
        let hits = idx.search(&v, 5).expect("search");
        assert_eq!(hits.first().map(|(s, _)| s.as_str()), Some(uri), "got {:?}", hits);
    }
}
