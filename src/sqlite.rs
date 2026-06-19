//! SQLite implementation of MemoryStore. Canonical records in `entries`, append-only and
//! bitemporal: every save is a new version (prior version closed in system time, never
//! deleted), with independent valid-time (true-in-world) and system-time (recorded-at)
//! axes. Keyword recall via an FTS5 index that holds ONLY the current version of each
//! record. As-of queries reconstruct any past slice. See `Entry` for the temporal model.

use crate::entry::{Entry, Kind};
use crate::store::MemoryStore;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

/// Full column list (read order matches `row_to_entry`, which reads by name).
const COLS: &str = "uri,kind,namespace,title,body,tags,importance,dedup_key,\
created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms";

/// The "current slice" predicate: the currently-recorded version (system_to NULL) that is
/// still true-in-world at the bound `now` param. `?` placeholders are filled per query.
const CURRENT: &str = "system_to_ms IS NULL AND (valid_to_ms IS NULL OR valid_to_ms > ?)";

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        // The CREATE TABLE is the v1 (bitemporal) shape for fresh dbs; on an existing v0 db
        // it is a no-op and migrate() adds the new columns. Only columns present in BOTH v0
        // and v1 may be indexed here; the new-column indexes are created in migrate().
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS entries (
                id             INTEGER PRIMARY KEY,
                uri            TEXT NOT NULL,
                kind           TEXT NOT NULL,
                namespace      TEXT NOT NULL,
                title          TEXT NOT NULL,
                body           TEXT NOT NULL,
                tags           TEXT NOT NULL DEFAULT '[]',
                importance     INTEGER NOT NULL DEFAULT 50,
                dedup_key      TEXT NOT NULL,
                created_ms     INTEGER NOT NULL,
                valid_from_ms  INTEGER NOT NULL DEFAULT 0,
                valid_to_ms    INTEGER,
                system_from_ms INTEGER NOT NULL DEFAULT 0,
                system_to_ms   INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_entries_dedup ON entries(dedup_key);
            CREATE INDEX IF NOT EXISTS idx_entries_kind  ON entries(kind);
            CREATE INDEX IF NOT EXISTS idx_entries_valid ON entries(valid_to_ms);
            CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(idref UNINDEXED, text);
            CREATE TABLE IF NOT EXISTS signals (
                uri            TEXT PRIMARY KEY,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_access_ms INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .context("init schema")?;
        let store = Self { conn };
        store.migrate().context("migrate schema to bitemporal (v1)")?;
        Ok(store)
    }

    /// Migrate a v0 (soft-close) db to v1 (bitemporal), guarded by `PRAGMA user_version`.
    /// Idempotent and transactional: a v0 db is either fully migrated or left untouched.
    fn migrate(&self) -> Result<()> {
        let v: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v >= 1 {
            return Ok(());
        }
        let cols: Vec<String> = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(entries)")?;
            let r = stmt.query_map([], |row| row.get::<_, String>(1))?;
            r.filter_map(|x| x.ok()).collect()
        };
        let missing = |c: &str| !cols.iter().any(|x| x == c);
        self.conn.execute_batch("BEGIN")?;
        let res = (|| -> Result<()> {
            if missing("valid_from_ms") {
                self.conn.execute_batch("ALTER TABLE entries ADD COLUMN valid_from_ms INTEGER")?;
            }
            if missing("system_from_ms") {
                self.conn.execute_batch("ALTER TABLE entries ADD COLUMN system_from_ms INTEGER")?;
            }
            if missing("system_to_ms") {
                self.conn.execute_batch("ALTER TABLE entries ADD COLUMN system_to_ms INTEGER")?;
            }
            // Backfill the new lower bounds from creation time, then re-map v0 soft-close
            // semantics: a non-null valid_to_ms meant "superseded" (a system-time close,
            // not a valid-time end), so move it to system_to_ms and clear valid_to_ms.
            self.conn.execute_batch(
                "UPDATE entries SET valid_from_ms  = created_ms WHERE valid_from_ms  IS NULL;
                 UPDATE entries SET system_from_ms = created_ms WHERE system_from_ms IS NULL;
                 UPDATE entries SET system_to_ms = valid_to_ms, valid_to_ms = NULL \
                     WHERE valid_to_ms IS NOT NULL;",
            )?;
            // New-column indexes (safe now that the columns exist).
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_entries_uri    ON entries(uri);
                 CREATE INDEX IF NOT EXISTS idx_entries_syscur ON entries(system_to_ms);",
            )?;
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.conn.execute_batch("PRAGMA user_version = 1; COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
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
            valid_from_ms: row.get("valid_from_ms")?,
            valid_to_ms: row.get("valid_to_ms")?,
            system_from_ms: row.get("system_from_ms")?,
            system_to_ms: row.get("system_to_ms")?,
        })
    }

    fn get_by_id(&self, id: i64) -> Result<Option<Entry>> {
        let now = crate::entry::now_ms();
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLS} FROM entries WHERE id=?1 AND {CURRENT}"))?;
        let mut rows = stmt.query(params![id, now])?;
        if let Some(r) = rows.next()? {
            Ok(Some(Self::row_to_entry(r)?))
        } else {
            Ok(None)
        }
    }

    /// Fetch the current (live) entry for a uri (used by RRF fusion to hydrate vector hits).
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub fn get(&self, uri: &str) -> Result<Option<Entry>> {
        let now = crate::entry::now_ms();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND {CURRENT} ORDER BY system_from_ms DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![uri, now])?;
        if let Some(r) = rows.next()? {
            Ok(Some(Self::row_to_entry(r)?))
        } else {
            Ok(None)
        }
    }

    /// Bump the runtime access signal for a uri (called best-effort after recall).
    pub fn bump_signal(&self, uri: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO signals(uri, access_count, last_access_ms) VALUES(?1, 1, ?2) \
             ON CONFLICT(uri) DO UPDATE SET access_count = access_count + 1, last_access_ms = ?2",
            params![uri, now_ms],
        )?;
        Ok(())
    }

    /// Read (access_count, last_access_ms) for a set of uris (absent uris omitted).
    pub fn read_signals(&self, uris: &[String]) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        let mut out = std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT access_count, last_access_ms FROM signals WHERE uri=?1")?;
        for uri in uris {
            let mut rows = stmt.query(params![uri])?;
            if let Some(r) = rows.next()? {
                out.insert(uri.clone(), (r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
            }
        }
        Ok(out)
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
        // Append-only supersede: close prior CURRENT versions (same dedup_key) in SYSTEM
        // time only (system_to_ms = now) - the old version is retained, never deleted - and
        // drop their FTS rows so keyword recall sees only the current version.
        let mut sel = self
            .conn
            .prepare("SELECT id FROM entries WHERE dedup_key=?1 AND system_to_ms IS NULL")?;
        let ids: Vec<i64> = sel
            .query_map(params![e.dedup_key], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        for id in &ids {
            self.conn
                .execute("UPDATE entries SET system_to_ms=?1 WHERE id=?2", params![now, id])?;
            self.conn
                .execute("DELETE FROM entries_fts WHERE idref=?1", params![id])?;
        }
        // Append the new current version: system_from = now (store-authoritative), system_to
        // = NULL. created_ms / valid_from_ms / valid_to_ms come from the entry.
        let tags = serde_json::to_string(&e.tags).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,\
             created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL)",
            params![
                e.uri, e.kind.as_str(), e.namespace, e.title, e.body, tags,
                e.importance, e.dedup_key, e.created_ms, e.valid_from_ms, e.valid_to_ms, now
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
        let now = crate::entry::now_ms();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {CURRENT} \
             ORDER BY importance DESC, created_ms DESC LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![now, limit as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Entry>> {
        let now = crate::entry::now_ms();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE kind=?1 AND {CURRENT} \
             ORDER BY importance DESC, created_ms DESC LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![kind, now, limit as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>> {
        // As-of can't use the FTS index (it holds only the current version of each record),
        // so scan `entries` for the as-of slice and keyword-filter in Rust. History is small
        // per tenant, so a linear scan is fine; this keeps as-of deterministic and simple.
        let pred = "system_from_ms <= ?1 AND (system_to_ms IS NULL OR system_to_ms > ?1) \
                    AND valid_from_ms <= ?2 AND (valid_to_ms IS NULL OR valid_to_ms > ?2)";
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {pred} ORDER BY importance DESC, created_ms DESC"
        ))?;
        let all: Vec<Entry> = stmt
            .query_map(params![as_of_ms, valid_ms], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .collect();
        let terms: Vec<String> = query
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| t.len() >= 2)
            .map(|t| t.to_lowercase())
            .collect();
        if terms.is_empty() {
            return Ok(all.into_iter().take(limit).collect());
        }
        let matched = all
            .into_iter()
            .filter(|e| {
                let hay = format!("{} {} {}", e.title, e.body, e.tags.join(" ")).to_lowercase();
                terms.iter().any(|t| hay.contains(t))
            })
            .take(limit)
            .collect();
        Ok(matched)
    }

    fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 ORDER BY system_from_ms DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![uri, limit as i64], Self::row_to_entry)?
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
        Entry::new_now(uri.clone(), kind, ns.into(), title.into(), body.into(), vec![], 50, uri)
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

    #[test]
    fn signals_bump_and_read() {
        let s = mem_store();
        s.bump_signal("daimon://a", 1000).unwrap();
        s.bump_signal("daimon://a", 2000).unwrap();
        let m = s
            .read_signals(&["daimon://a".to_string(), "daimon://missing".to_string()])
            .unwrap();
        assert_eq!(m.get("daimon://a").copied(), Some((2, 2000)));
        assert!(!m.contains_key("daimon://missing"));
    }

    #[test]
    fn supersede_is_append_only_and_versioned() {
        let s = mem_store();
        let mut e = mk(Kind::Decision, "resources/x", "Same Title", "first body");
        s.put(&e).unwrap();
        e.body = "second body".into();
        s.put(&e).unwrap();
        // current slice still has exactly one live version, with the new body
        let live: Vec<_> = s.recent(10).unwrap().into_iter().filter(|x| x.uri == e.uri).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].body, "second body");
        // ...but BOTH physical versions are retained (append-only, not overwritten)
        let physical: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM entries WHERE uri=?1", params![e.uri], |r| r.get(0))
            .unwrap();
        assert_eq!(physical, 2, "old version must be retained, not deleted");
    }

    #[test]
    fn as_of_returns_historical_version_and_history_lists_all() {
        let s = mem_store();
        let mut e = mk(Kind::Decision, "resources/x", "Vector substrate", "we picked lancedb first");
        s.put(&e).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(8));
        let t_mid = now_ms(); // strictly between the two system_from stamps
        std::thread::sleep(std::time::Duration::from_millis(8));
        e.body = "we switched to zvec second".into();
        s.put(&e).unwrap();

        // current recall sees the latest version
        let now_hits = s.recall("lancedb zvec", 10).unwrap();
        assert!(now_hits.iter().any(|x| x.body.contains("zvec")), "current = latest");

        // as-of the midpoint sees the FIRST version (zvec did not yet exist then)
        let past = s.recall_as_of("lancedb zvec", 10, t_mid, t_mid).unwrap();
        assert_eq!(past.len(), 1, "exactly the as-of-current version");
        assert!(past[0].body.contains("lancedb") && !past[0].body.contains("zvec"));

        // history lists both versions, newest first
        let hist = s.history(&e.uri, 10).unwrap();
        assert_eq!(hist.len(), 2);
        assert!(hist[0].body.contains("zvec") && hist[1].body.contains("lancedb"));
    }

    #[test]
    fn migrates_v0_soft_close_to_bitemporal() {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmmig-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        // hand-build a v0 (pre-bitemporal) db: one live row, one soft-closed row
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE entries (id INTEGER PRIMARY KEY, uri TEXT NOT NULL, kind TEXT NOT NULL,
                    namespace TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
                    tags TEXT NOT NULL DEFAULT '[]', importance INTEGER NOT NULL DEFAULT 50,
                    dedup_key TEXT NOT NULL, created_ms INTEGER NOT NULL, valid_to_ms INTEGER);
                 CREATE VIRTUAL TABLE entries_fts USING fts5(idref UNINDEXED, text);
                 PRAGMA user_version = 0;",
            )
            .unwrap();
            c.execute(
                "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms) \
                 VALUES('daimon://live','memory','ns','Live','b','[]',50,'daimon://live',1000,NULL)",
                [],
            ).unwrap();
            c.execute(
                "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,created_ms,valid_to_ms) \
                 VALUES('daimon://closed','memory','ns','Closed','b','[]',50,'daimon://closed',1000,2000)",
                [],
            ).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1, "user_version bumped to 1");
        let total: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 2, "append-only: both rows survive migration");
        // current slice = only the formerly-live row; the soft-closed one became a closed
        // SYSTEM-time version (system_to_ms set), so it drops out of the current slice.
        let recent = s.recent(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].title, "Live");
        // re-opening is a no-op (idempotent)
        let s2 = SqliteStore::open(&path).unwrap();
        assert_eq!(s2.recent(10).unwrap().len(), 1);
    }
}
