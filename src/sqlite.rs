//! SQLite implementation of MemoryStore (M0). Canonical records in `entries`
//! (close-not-delete via valid_to_ms), keyword recall via an FTS5 index. Vectors are
//! intentionally absent in M0 (offline-default keyword-only); they layer in later as a
//! rebuildable index or via the LanceDB impl, behind the same trait.

use crate::entry::{Entry, Kind};
use crate::store::MemoryStore;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS entries (
                id          INTEGER PRIMARY KEY,
                uri         TEXT NOT NULL,
                kind        TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                title       TEXT NOT NULL,
                body        TEXT NOT NULL,
                tags        TEXT NOT NULL DEFAULT '[]',
                importance  INTEGER NOT NULL DEFAULT 50,
                dedup_key   TEXT NOT NULL,
                created_ms  INTEGER NOT NULL,
                valid_to_ms INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_entries_dedup ON entries(dedup_key);
            CREATE INDEX IF NOT EXISTS idx_entries_kind  ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_live  ON entries(valid_to_ms);
            CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(idref UNINDEXED, text);
            "#,
        )
        .context("init schema")?;
        Ok(Self { conn })
    }

    fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<Entry> {
        let kind_s: String = row.get("kind")?;
        let tags_s: String = row.get("tags")?;
        let tags: Vec<String> = serde_json::from_str(&tags_s).unwrap_or_default();
        Ok(Entry {
            uri: row.get("uri")?,
            kind: Kind::from_str(&kind_s).unwrap_or(Kind::Memory),
            namespace: row.get("namespace")?,
            title: row.get("title")?,
            body: row.get("body")?,
            tags,
            importance: row.get("importance")?,
            dedup_key: row.get("dedup_key")?,
            created_ms: row.get("created_ms")?,
            valid_to_ms: row.get("valid_to_ms")?,
        })
    }

    fn get_by_id(&self, id: i64) -> Result<Option<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms \
             FROM entries WHERE id=?1 AND valid_to_ms IS NULL",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(r) = rows.next()? {
            Ok(Some(Self::row_to_entry(r)?))
        } else {
            Ok(None)
        }
    }
}

/// Build a safe FTS5 query: quote each alphanumeric term, OR them together.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

impl MemoryStore for SqliteStore {
    fn put(&self, e: &Entry) -> Result<()> {
        let now = crate::entry::now_ms();
        // Close prior live records with the same dedup_key, and drop their FTS rows.
        let mut sel = self
            .conn
            .prepare("SELECT id FROM entries WHERE dedup_key=?1 AND valid_to_ms IS NULL")?;
        let ids: Vec<i64> = sel
            .query_map(params![e.dedup_key], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        for id in &ids {
            self.conn
                .execute("UPDATE entries SET valid_to_ms=?1 WHERE id=?2", params![now, id])?;
            self.conn
                .execute("DELETE FROM entries_fts WHERE idref=?1", params![id])?;
        }
        // Insert the new live record.
        let tags = serde_json::to_string(&e.tags).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)",
            params![
                e.uri, e.kind.as_str(), e.namespace, e.title, e.body, tags,
                e.importance, e.dedup_key, e.created_ms
            ],
        )?;
        let new_id = self.conn.last_insert_rowid();
        let fts_text = format!("{} {} {}", e.title, e.body, e.tags.join(" "));
        self.conn.execute(
            "INSERT INTO entries_fts(idref, text) VALUES (?1, ?2)",
            params![new_id, fts_text],
        )?;
        Ok(())
    }

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        let fq = match fts_query(query) {
            Some(q) => q,
            None => return self.recent(limit),
        };
        let mut stmt = self
            .conn
            .prepare("SELECT idref FROM entries_fts WHERE entries_fts MATCH ?1 ORDER BY rank LIMIT ?2")?;
        let ids: Vec<i64> = stmt
            .query_map(params![fq, limit as i64], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let mut out = Vec::new();
        for id in ids {
            if let Some(e) = self.get_by_id(id)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms \
             FROM entries WHERE valid_to_ms IS NULL \
             ORDER BY importance DESC, created_ms DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms \
             FROM entries WHERE kind=?1 AND valid_to_ms IS NULL \
             ORDER BY importance DESC, created_ms DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![kind, limit as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{make_uri, now_ms};

    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn mem_store() -> SqliteStore {
        // unique path per call so parallel tests never share a file (WAL lock otherwise)
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmtest-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        SqliteStore::open(&dir.join("t.db")).unwrap()
    }

    fn mk(kind: Kind, ns: &str, title: &str, body: &str) -> Entry {
        let uri = make_uri(ns, kind, title);
        Entry {
            uri: uri.clone(),
            kind,
            namespace: ns.into(),
            title: title.into(),
            body: body.into(),
            tags: vec![],
            importance: 50,
            dedup_key: uri,
            created_ms: now_ms(),
            valid_to_ms: None,
        }
    }

    #[test]
    fn put_and_recall_roundtrip() {
        let s = mem_store();
        s.put(&mk(Kind::Decision, "resources/x", "Lock LanceDB substrate", "we chose LanceDB for v2 vector storage")).unwrap();
        s.put(&mk(Kind::Lesson, "agent/lessons", "AVX2 gate", "the embedder needs AVX2 cpu instructions")).unwrap();
        let hits = s.recall("lancedb substrate", 5).unwrap();
        assert!(hits.iter().any(|e| e.title.contains("LanceDB")), "should recall the LanceDB decision");
        let hits2 = s.recall("avx2", 5).unwrap();
        assert!(hits2.iter().any(|e| e.title == "AVX2 gate"));
    }

    #[test]
    fn dedup_supersede_keeps_one_live() {
        let s = mem_store();
        let mut e = mk(Kind::Decision, "resources/x", "Same Title", "first body");
        s.put(&e).unwrap();
        e.body = "second body".into();
        s.put(&e).unwrap(); // same dedup_key -> supersede
        let recent = s.recent(10).unwrap();
        let live: Vec<_> = recent.iter().filter(|x| x.uri == e.uri).collect();
        assert_eq!(live.len(), 1, "exactly one live version after supersede");
        assert_eq!(live[0].body, "second body");
    }

    #[test]
    fn empty_query_returns_recent() {
        let s = mem_store();
        s.put(&mk(Kind::Memory, "resources/x", "alpha", "a")).unwrap();
        assert_eq!(s.recall("", 10).unwrap().len(), 1);
    }
}
