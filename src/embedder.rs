//! Embedder trait + a dependency-free placeholder. The vector store (zvec) stores and
//! searches vectors; SOMETHING must turn text into a vector. `HashEmbedder` is a
//! deterministic, offline bag-of-hashed-tokens vector - enough to WIRE and TEST the zvec
//! vector path end-to-end with no model download. It approximates keyword overlap, NOT
//! real semantics; a real model (bge-small via fastembed/candle, behind this same trait)
//! is the next step. Kept honest: this is a placeholder embedder.
#![allow(dead_code)]

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
}
