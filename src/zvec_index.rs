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

    /// Upsert (supersede) the vector for a uri.
    pub fn upsert(&self, uri: &str, vector: &[f32]) -> Result<()> {
        let _ = self.collection.delete(&[uri]); // best-effort close prior
        let mut doc = Doc::new().map_err(ze)?;
        doc.set_pk(uri);
        doc.add_string("uri", uri).map_err(ze)?;
        doc.add_vector_f32("embedding", vector).map_err(ze)?;
        self.collection.insert(&[&doc]).map_err(ze)?;
        Ok(())
    }

    /// Nearest uris to the query vector, best first.
    pub fn search(&self, vector: &[f32], k: usize) -> Result<Vec<String>> {
        let q = SearchQuery::new("embedding", vector, k as i32).map_err(ze)?;
        let results = self.collection.query(&q).map_err(ze)?;
        Ok(results
            .iter()
            .filter_map(|r| r.get_pk().map(|s| s.to_string()))
            .collect())
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
            hits.first().map(|s| s == "uri-a").unwrap_or(false),
            "nearest to a should be uri-a, got {:?}",
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
            hits.contains(&"uri-a".to_string()),
            "reopened index should still find uri-a, got {:?}",
            hits
        );
    }
}
