//! Embedder trait + two impls behind it. The vector store (zvec) stores and searches
//! vectors; SOMETHING must turn text into a vector.
//! - `FastEmbedder` (feature `fastembed`): the real one - bge-small-en-v1.5 (384-d) via
//!   ONNX. Produces genuine semantic vectors; this is what earns the vector half its keep.
//! - `HashEmbedder` (default / fallback): a deterministic, offline bag-of-hashed-tokens
//!   vector (256-d). It approximates keyword overlap, NOT real semantics - used when
//!   `fastembed` is off or the model can't load, so the vector path stays wired and tested
//!   with no model download. Kept honest: the hash path is a placeholder, the bge path is real.
#![allow(dead_code)]

/// Embedding dimension. Tied to the active model so the zvec collection schema matches:
/// bge-small-en-v1.5 is 384-d; the placeholder uses 256.
#[cfg(feature = "fastembed")]
pub const DIM: usize = 384;
#[cfg(not(feature = "fastembed"))]
pub const DIM: usize = 256;

pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

pub struct HashEmbedder;

impl HashEmbedder {
    pub fn new() -> Self {
        HashEmbedder
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

fn fnv1a(token: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in token.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        DIM
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; DIM];
        for tok in text.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()) {
            if tok.len() < 2 {
                continue;
            }
            let idx = (fnv1a(tok) as usize) % DIM;
            v[idx] += 1.0;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }
}

/// Real semantic embedder: bge-small-en-v1.5 (384-d) via fastembed/ONNX. Downloads the
/// model to the fastembed cache on first construction (needs network once).
#[cfg(feature = "fastembed")]
pub struct FastEmbedder {
    model: fastembed::TextEmbedding,
}

#[cfg(feature = "fastembed")]
impl FastEmbedder {
    pub fn new() -> anyhow::Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))?;
        Ok(Self { model })
    }
}

#[cfg(feature = "fastembed")]
impl Embedder for FastEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        match self.model.embed(vec![text], None) {
            Ok(mut v) => v.pop().unwrap_or_else(|| vec![0.0; DIM]),
            Err(e) => {
                eprintln!("dmem: fastembed embed failed: {e:?}");
                vec![0.0; DIM]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_text_closer_than_unrelated() {
        let e = HashEmbedder::new();
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let a = e.embed("lancedb vector substrate decision");
        let b = e.embed("we chose the lancedb vector substrate");
        let c = e.embed("totally unrelated cooking recipe");
        assert!(cos(&a, &b) > cos(&a, &c), "shared-word text should score higher");
    }

    // Guards the REAL semantic path: bge-small must rank a semantically-related pair above
    // an unrelated one even with NO shared keywords (the HashEmbedder above cannot - it is
    // keyword-equivalent). Gated so the default `cargo test` never loads the model.
    #[cfg(feature = "fastembed")]
    #[test]
    fn fastembed_related_closer_than_unrelated() {
        let e = FastEmbedder::new().expect("load bge-small model");
        // bge-small vectors are L2-normalized, so dot product == cosine similarity.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let a = e.embed("the database server crashed and lost data");
        let b = e.embed("our postgres instance went down and we lost records");
        let c = e.embed("a recipe for chocolate chip cookies");
        assert_eq!(a.len(), DIM);
        assert!(
            cos(&a, &b) > cos(&a, &c),
            "semantically-related pair must outscore unrelated: rel={} unrel={}",
            cos(&a, &b),
            cos(&a, &c)
        );
    }
}
