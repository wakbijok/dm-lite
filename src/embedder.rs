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
#[cfg(any(feature = "fastembed", feature = "candle"))]
pub const DIM: usize = 384;
#[cfg(not(any(feature = "fastembed", feature = "candle")))]
pub const DIM: usize = 256;

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Stable identity of the active embedder. Used by the recall relevance floor (cosine
    /// magnitudes are embedder-relative, so the floor must know which model produced them) and
    /// by the calibration harness to ASSERT the real model loaded (a silent HashEmbedder
    /// fallback would make every cosine number bogus). "hash" marks the placeholder, whose
    /// cosine approximates keyword overlap, NOT bge-scale semantics.
    fn name(&self) -> &'static str;
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
    fn name(&self) -> &'static str {
        "hash"
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
    fn name(&self) -> &'static str {
        "fastembed-bge-small"
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

/// Tiny STATIC embedder: a Model2Vec token->vector table (NO neural net, NO ONNX runtime).
/// Loaded via model2vec-rs from a `.safetensors` model; embedding is tokenize + mean-pool
/// lookup over `ndarray`. The model id is overridable via `DM_M2V_MODEL` so we can benchmark
/// variants (potion-base-8M, retrieval, multilingual) without recompiling. Output is
/// L2-normalized (normalize=true) so dot product == cosine, matching the recall fusion.
#[cfg(feature = "model2vec")]
pub struct Model2VecEmbedder {
    model: model2vec_rs::model::StaticModel,
}

#[cfg(feature = "model2vec")]
impl Model2VecEmbedder {
    pub fn new() -> anyhow::Result<Self> {
        let repo = std::env::var("DM_M2V_MODEL").unwrap_or_else(|_| "minishlab/potion-base-8M".to_string());
        let model = model2vec_rs::model::StaticModel::from_pretrained(repo.as_str(), None, Some(true), None)
            .map_err(|e| anyhow::anyhow!("model2vec load {repo}: {e:#}"))?;
        Ok(Self { model })
    }
}

#[cfg(feature = "model2vec")]
impl Embedder for Model2VecEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    fn name(&self) -> &'static str {
        "model2vec"
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = self.model.encode_single(text);
        // Benchmark guard: a model whose native dim != DIM still measures RAM correctly; for the
        // chosen end-to-end model (potion-base-8M = 256-d = DIM) the dims match and this is a no-op.
        if v.len() != DIM {
            v.resize(DIM, 0.0);
        }
        v
    }
}

/// Same model as FastEmbedder (bge-small-en-v1.5, 384-d) but run on **Candle** (pure-Rust ML),
/// NOT ONNX Runtime. Identical weights -> identical accuracy; the point is dropping the heavy
/// ONNX C++ runtime to cut RAM while keeping a real transformer. CLS pooling + L2 normalize
/// (bge's documented pooling). Model id overridable via DM_CANDLE_MODEL for benchmarking.
#[cfg(feature = "candle")]
pub struct CandleEmbedder {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: candle_core::Device,
}

#[cfg(feature = "candle")]
impl CandleEmbedder {
    pub fn new() -> anyhow::Result<Self> {
        use candle_nn::VarBuilder;
        use candle_transformers::models::bert::{BertModel, Config, DTYPE};
        use hf_hub::api::sync::Api;
        let repo = std::env::var("DM_CANDLE_MODEL").unwrap_or_else(|_| "BAAI/bge-small-en-v1.5".to_string());
        let r = Api::new()?.model(repo);
        let config: Config = serde_json::from_str(&std::fs::read_to_string(r.get("config.json")?)?)?;
        let tokenizer = tokenizers::Tokenizer::from_file(r.get("tokenizer.json")?)
            .map_err(|e| anyhow::anyhow!("tokenizer: {e}"))?;
        let device = candle_core::Device::Cpu;
        let weights = r.get("model.safetensors")?;
        // DM_CANDLE_F16=1 loads weights as f16 (half the model RAM). from_mmaped keeps the file's
        // f32 bytes zero-copy, so we load buffered and convert to the target dtype instead.
        let dtype = if std::env::var("DM_CANDLE_F16").is_ok() { candle_core::DType::F16 } else { DTYPE };
        let vb = if dtype == DTYPE {
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)? }
        } else {
            let bytes = std::fs::read(&weights)?;
            VarBuilder::from_buffered_safetensors(bytes, dtype, &device)?
        };
        let model = BertModel::load(vb, &config)?;
        Ok(Self { model, tokenizer, device })
    }

    fn embed_inner(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        use candle_core::{IndexOp, Tensor};
        let enc = self.tokenizer.encode(text, true).map_err(|e| anyhow::anyhow!("encode: {e}"))?;
        let ids = Tensor::new(enc.get_ids(), &self.device)?.unsqueeze(0)?;
        let type_ids = ids.zeros_like()?;
        let mask = Tensor::new(enc.get_attention_mask(), &self.device)?.unsqueeze(0)?;
        let out = self.model.forward(&ids, &type_ids, Some(&mask))?; // [1, seq, hidden]
        let cls: Vec<f32> = out.i((0, 0))?.to_vec1()?; // CLS token
        let norm = cls.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(if norm > 0.0 { cls.iter().map(|x| x / norm).collect() } else { cls })
    }
}

#[cfg(feature = "candle")]
impl Embedder for CandleEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    fn name(&self) -> &'static str {
        "candle-bge-small"
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        match self.embed_inner(text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("dmem: candle embed failed: {e:?}");
                vec![0.0; DIM]
            }
        }
    }
}

/// Read-only diagnostics about the embedder that WOULD load (mirrors `build_embedder`'s selection
/// order WITHOUT constructing it, so `dmem doctor` never triggers a model download). For neural
/// embedders it reports the HuggingFace hub cache dir and whether the model is already present,
/// so an air-gapped operator can pre-populate it.
pub struct EmbedderDiag {
    pub name: &'static str,
    pub model_id: Option<String>,
    pub neural: bool,
    pub cache_dir: Option<std::path::PathBuf>,
    pub cache_present: bool,
}

/// The HuggingFace hub cache dir (HUGGINGFACE_HUB_CACHE, else HF_HOME/hub, else
/// ~/.cache/huggingface/hub, matching hf-hub's own resolution), and whether `model_id`'s snapshot
/// is already cached there.
#[cfg(any(feature = "fastembed", feature = "candle", feature = "model2vec"))]
fn hf_model_cache(model_id: &str) -> (Option<std::path::PathBuf>, bool) {
    let hub = if let Ok(c) = std::env::var("HUGGINGFACE_HUB_CACHE") {
        std::path::PathBuf::from(c)
    } else if let Ok(h) = std::env::var("HF_HOME") {
        std::path::PathBuf::from(h).join("hub")
    } else if let Some(h) = dirs::home_dir() {
        h.join(".cache").join("huggingface").join("hub")
    } else {
        return (None, false);
    };
    let model_dir = hub.join(format!("models--{}", model_id.replace('/', "--")));
    let present = model_dir.join("snapshots").is_dir();
    (Some(hub), present)
}

/// Diagnostics for the active embedder, computed without loading the model.
pub fn active_embedder_diag() -> EmbedderDiag {
    #[cfg(feature = "fastembed")]
    {
        let m = std::env::var("DM_CANDLE_MODEL").unwrap_or_else(|_| "BAAI/bge-small-en-v1.5".to_string());
        let (dir, present) = hf_model_cache(&m);
        return EmbedderDiag { name: "fastembed-bge-small", model_id: Some(m), neural: true, cache_dir: dir, cache_present: present };
    }
    #[cfg(all(feature = "candle", not(feature = "fastembed")))]
    {
        let m = std::env::var("DM_CANDLE_MODEL").unwrap_or_else(|_| "BAAI/bge-small-en-v1.5".to_string());
        let (dir, present) = hf_model_cache(&m);
        return EmbedderDiag { name: "candle-bge-small", model_id: Some(m), neural: true, cache_dir: dir, cache_present: present };
    }
    #[cfg(all(feature = "model2vec", not(feature = "fastembed"), not(feature = "candle")))]
    {
        let m = std::env::var("DM_M2V_MODEL").unwrap_or_else(|_| "minishlab/potion-base-8M".to_string());
        let (dir, present) = hf_model_cache(&m);
        return EmbedderDiag { name: "model2vec", model_id: Some(m), neural: true, cache_dir: dir, cache_present: present };
    }
    #[allow(unreachable_code)]
    EmbedderDiag { name: "hash-256", model_id: None, neural: false, cache_dir: None, cache_present: false }
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
        // The related pair shares NO content tokens, so a keyword-equivalent embedder
        // (HashEmbedder) would score them at 0 and FAIL this - only real semantics pass.
        let sent_a = "the production database stopped responding";
        let sent_b = "our postgres node became unreachable";
        let sent_c = "a recipe for chocolate chip cookies";
        let toks = |s: &str| {
            s.split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|t| t.len() >= 2)
                .map(|t| t.to_lowercase())
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(
            toks(sent_a).is_disjoint(&toks(sent_b)),
            "related pair must share no keywords, else the test doesn't isolate semantics from overlap"
        );

        let e = FastEmbedder::new().expect("load bge-small model");
        // bge-small vectors are L2-normalized, so dot product == cosine similarity.
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let a = e.embed(sent_a);
        let b = e.embed(sent_b);
        let c = e.embed(sent_c);
        assert_eq!(a.len(), DIM);
        assert!(
            cos(&a, &b) > cos(&a, &c),
            "semantically-related pair must outscore unrelated: rel={} unrel={}",
            cos(&a, &b),
            cos(&a, &c)
        );
    }
}
