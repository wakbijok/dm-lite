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

/// A record's body may span several embedding windows (see Embedder::embed_chunks). Each window is
/// a distinct vector under its own PK, but all carry the same `uri` field, so search max-pools them
/// back to one record. Chunk 0's PK IS `pk_for(uri)` (the historical single-vector key), so a
/// single-chunk record is byte-identical to the pre-chunking scheme and needs no migration; chunks
/// 1.. append a 2-hex suffix (PK stays well under zvec's 64-byte cap).
const MAX_CHUNKS: usize = 64;
fn chunk_pk(uri: &str, i: usize) -> String {
    if i == 0 {
        pk_for(uri)
    } else {
        format!("{}{:02x}", pk_for(uri), i)
    }
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

    /// Upsert (supersede) a uri's vectors, one per body window. Clears any prior chunks first
    /// (including stale ones from a longer previous version), so re-saving is exact. A single
    /// vector reduces to the historical one-vector-per-uri behavior.
    pub fn upsert_chunks(&self, uri: &str, vectors: &[Vec<f32>]) -> Result<()> {
        // Best-effort clear the full chunk range so a shrink leaves no orphans.
        let stale: Vec<String> = (0..MAX_CHUNKS).map(|i| chunk_pk(uri, i)).collect();
        let _ = self.collection.delete(&stale.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut docs = Vec::with_capacity(vectors.len());
        for (i, v) in vectors.iter().take(MAX_CHUNKS).enumerate() {
            let mut doc = Doc::new().map_err(ze)?;
            doc.set_pk(&chunk_pk(uri, i));
            doc.add_string("uri", uri).map_err(ze)?;
            doc.add_vector_f32("embedding", v).map_err(ze)?;
            docs.push(doc);
        }
        let refs: Vec<&Doc> = docs.iter().collect();
        self.collection.insert(&refs).map_err(ze)?;
        Ok(())
    }

    /// Remove all of a uri's chunk vectors (used by forget). Best-effort idempotent.
    pub fn remove(&self, uri: &str) -> Result<()> {
        let pks: Vec<String> = (0..MAX_CHUNKS).map(|i| chunk_pk(uri, i)).collect();
        let _ = self.collection.delete(&pks.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        Ok(())
    }

    /// Nearest (uri, similarity) to the query vector, best first. zvec's `get_score()` for a
    /// MetricType::Cosine HNSW index returns a cosine DISTANCE (0.0 = identical, 1.0 = orthogonal,
    /// 2.0 = opposite; smaller = nearer), verified empirically. Our embeddings are L2-normalized,
    /// so we convert to a true cosine SIMILARITY = 1.0 - distance, giving [-1, 1] with HIGHER =
    /// more similar - the form the recall relevance floor (abs_cosine) thresholds on. Surfacing
    /// this magnitude (not just the uri) is what lets the floor drop semantically-distant hits.
    pub fn search(&self, vector: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        // A record can occupy several chunk vectors; over-fetch so that after collapsing chunks to
        // their parent record we still surface ~k distinct records.
        let raw_k = (k * 8).max(64);
        let q = SearchQuery::builder()
            .field_name("embedding")
            .vector(vector)
            .topk(raw_k as i32)
            .output_fields(&["uri"])
            .build()
            .map_err(ze)?;
        let results = self.collection.query(&q).map_err(ze)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in &results {
            if let Ok(Some(uri)) = r.get_string("uri") {
                // First occurrence of a uri is its best chunk (results are best-first) -> max-pool.
                if seen.insert(uri.clone()) {
                    out.push((uri, 1.0 - r.get_score()));
                    if out.len() >= k {
                        break;
                    }
                }
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
        idx.upsert_chunks("uri-a", std::slice::from_ref(&a)).expect("upsert a");
        idx.upsert_chunks("uri-b", std::slice::from_ref(&b)).expect("upsert b");
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
            idx.upsert_chunks("uri-a", std::slice::from_ref(&a)).expect("upsert");
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
    fn chunked_record_maxpools_to_one_hit() {
        // a record with two orthogonal chunk vectors: a query matching EITHER chunk must surface
        // the record (the long-record fix), and it must appear exactly once (max-pool dedup).
        let dir = std::env::temp_dir().join(format!("zvecchunk-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut head = vec![0f32; DIM];
        head[0] = 1.0;
        let mut tail = vec![0f32; DIM];
        tail[100] = 1.0;
        let mut other = vec![0f32; DIM];
        other[200] = 1.0;
        idx.upsert_chunks("uri-doc", &[head.clone(), tail.clone()]).expect("upsert chunks");
        idx.upsert_chunks("uri-other", &[other]).expect("upsert other");
        // a query matching the TAIL chunk surfaces the record, even though the head is its chunk 0
        let hits = idx.search(&tail, 5).expect("search tail");
        assert_eq!(hits.first().map(|(u, _)| u.as_str()), Some("uri-doc"), "tail query should hit the doc, got {hits:?}");
        assert_eq!(hits.iter().filter(|(u, _)| u == "uri-doc").count(), 1, "doc must dedup to one hit, got {hits:?}");
        // and a query matching the HEAD chunk surfaces it too
        let hits_h = idx.search(&head, 5).expect("search head");
        assert!(hits_h.iter().any(|(u, _)| u == "uri-doc"), "head query should hit the doc, got {hits_h:?}");
    }

    #[test]
    fn shrinking_chunks_clears_stale() {
        // re-saving a record with fewer chunks must drop the orphaned tail vectors.
        let dir = std::env::temp_dir().join(format!("zvecshrink-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut a = vec![0f32; DIM];
        a[0] = 1.0;
        let mut b = vec![0f32; DIM];
        b[100] = 1.0;
        let mut c = vec![0f32; DIM];
        c[200] = 1.0;
        idx.upsert_chunks("uri-x", &[a.clone(), b, c.clone()]).expect("upsert 3");
        idx.upsert_chunks("uri-x", &[a.clone()]).expect("upsert 1"); // shrink to one chunk
        let hits = idx.search(&c, 5).expect("search c");
        // the exact c-chunk would score ~1.0 if it survived; after clearing, uri-x can still appear
        // (only the head chunk remains) but at the orthogonal ~0.0 score, proving the tail is gone.
        let cx = hits.iter().find(|(u, _)| u == "uri-x").map(|(_, s)| *s);
        assert!(cx.unwrap_or(0.0) < 0.5, "stale tail chunk (sim ~1.0) must be cleared; uri-x score for c was {cx:?}");
        let hits_a = idx.search(&a, 5).expect("search a");
        assert!(hits_a.iter().any(|(u, _)| u == "uri-x"), "surviving head chunk must still match, got {hits_a:?}");
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
        idx.upsert_chunks(uri, std::slice::from_ref(&v)).expect("upsert long daimon uri");
        let hits = idx.search(&v, 5).expect("search");
        assert_eq!(hits.first().map(|(s, _)| s.as_str()), Some(uri), "got {:?}", hits);
    }
}
