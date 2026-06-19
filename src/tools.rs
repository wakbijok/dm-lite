//! High-level memory API: the typed guided save tools (per-kind required-field
//! validation) + recall. This is daimon's distinctive layer over the engine.

use crate::config;
use crate::entry::{make_uri, now_ms, Entry, Kind};
use crate::sqlite::SqliteStore;
use crate::store::MemoryStore;
use anyhow::{anyhow, Result};

pub struct Memory {
    store: SqliteStore,
    #[cfg(feature = "zvec")]
    vindex: Option<crate::zvec_index::ZvecIndex>,
    #[cfg(feature = "zvec")]
    embedder: Box<dyn crate::embedder::Embedder>,
}

/// Pick the embedder: real bge-small (fastembed) if it loads, else the placeholder.
#[cfg(feature = "zvec")]
fn make_embedder() -> Box<dyn crate::embedder::Embedder> {
    #[cfg(feature = "fastembed")]
    {
        match crate::embedder::FastEmbedder::new() {
            Ok(e) => return Box::new(e),
            Err(err) => eprintln!("dmem: fastembed model unavailable ({err:#}); using placeholder embedder"),
        }
    }
    Box::new(crate::embedder::HashEmbedder::new())
}

fn require(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(anyhow!("missing required field: {}", field))
    } else {
        Ok(())
    }
}

fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    line.chars().take(80).collect::<String>()
}

impl Memory {
    pub fn open() -> Result<Self> {
        let path = config::db_path(&config::tenant())?;
        let store = SqliteStore::open(&path)?;
        #[cfg(feature = "zvec")]
        {
            let vdir = config::data_dir()?.join("vectors").join(config::tenant());
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
        let uri = make_uri(namespace, kind, title);
        let e = Entry {
            uri: uri.clone(),
            kind,
            namespace: namespace.to_string(),
            title: title.to_string(),
            body,
            tags,
            importance,
            dedup_key: uri.clone(),
            created_ms: now_ms(),
            valid_to_ms: None,
        };
        self.store.put(&e)?;
        #[cfg(feature = "zvec")]
        if let Some(vindex) = &self.vindex {
            // Fail open: a vector-index hiccup must never block the canonical SQLite save.
            let v = self.embedder.embed(&e.body);
            if let Err(err) = vindex.upsert(&e.uri, &v) {
                eprintln!("dmem: vector index upsert failed for {} ({err:#}); keyword recall unaffected", e.uri);
            }
        }
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
        self.save(Kind::Lesson, namespace, title, body, 60, vec!["lesson".into()])
    }

    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str) -> Result<String> {
        require(title, "title")?;
        require(impact, "impact")?;
        let body = format!(
            "# {}\n\n**Impact:** {}\n\n**Resolution:** {}\n",
            title, impact, resolution
        );
        self.save(Kind::Incident, namespace, title, body, 65, vec!["incident".into()])
    }

    pub fn remember(&self, text: &str, namespace: &str) -> Result<String> {
        require(text, "text")?;
        let title = first_line(text);
        self.save(Kind::Memory, namespace, &title, text.to_string(), 50, vec![])
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
        self.save(Kind::Convention, namespace, title, body, 65, vec!["convention".into()])
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
        self.store.recall(query, limit)
    }

    /// Hybrid recall: SQLite FTS (keyword) + zvec (dense vector), fused by RRF.
    #[cfg(feature = "zvec")]
    fn recall_hybrid(&self, query: &str, limit: usize, vindex: &crate::zvec_index::ZvecIndex) -> Result<Vec<Entry>> {
        use std::collections::HashMap;
        let pool = limit.max(10);
        let kw: Vec<String> = self.store.recall(query, pool)?.into_iter().map(|e| e.uri).collect();
        let qv = self.embedder.embed(query);
        let vec: Vec<String> = match vindex.search(&qv, pool) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("dmem: vector search failed ({e:#}); using keyword results only");
                Vec::new()
            }
        };
        let k = 60.0_f64;
        let mut score: HashMap<String, f64> = HashMap::new();
        for (rank, uri) in kw.iter().enumerate() {
            *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, uri) in vec.iter().enumerate() {
            *score.entry(uri.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        let mut ranked: Vec<(String, f64)> = score.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::new();
        for (uri, _) in ranked.into_iter().take(limit) {
            if let Some(e) = self.store.get(&uri)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        self.store.recent(limit)
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
}
