//! SQLite implementation of MemoryStore. Canonical records in `entries`, append-only and
//! bitemporal: every save is a new version (prior version closed in system time, never
//! deleted), with independent valid-time (true-in-world) and system-time (recorded-at)
//! axes. Keyword recall via an FTS5 index that holds ONLY the current version of each
//! record. As-of queries reconstruct any past slice. See `Entry` for the temporal model.

use crate::entry::{Edge, Entry, Kind};
use crate::store::MemoryStore;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

/// Full column list (read order matches `row_to_entry`, which reads by name).
const COLS: &str = "uri,kind,namespace,title,body,tags,importance,dedup_key,\
created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms,scope";

/// The "current slice" predicate: the currently-recorded version (system_to NULL) that is
/// still true-in-world at the bound `now` param. `?` placeholders are filled per query.
const CURRENT: &str =
    "system_to_ms IS NULL AND valid_from_ms <= ? AND (valid_to_ms IS NULL OR valid_to_ms > ?)";

pub struct SqliteStore {
    conn: Connection,
    /// Read visibility (scope primitive): None = full tenant (every scope - today's behavior
    /// and the default); Some(set) = only records whose scope is `''` (global) or in the set.
    /// v1 filters post-query with over-fetch, the same pattern as the kind=skill exclusion.
    read_scopes: Option<std::collections::HashSet<String>>,
    /// Agent-tree visibility (security audit 11-08-2026, High #1): Some(a) hides every
    /// record under another agent's `agents/<b>/...` namespace from ALL reads - recall,
    /// recent, history, get, graph traversal - not just the persona route. The shared pool
    /// (everything outside `agents/`) stays fully visible: the shared brain is the point;
    /// only the persona mechanism itself is protected. None = legacy/agent-less = no filter.
    agent_view: Option<String>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        // Tenant DBs hold all memory content; keep them owner-only (audit Medium #5).
        // SQLite copies the main db file's mode onto -wal/-shm, so this covers those too.
        // Best-effort: a read-only FS or exotic mount must not brick the open.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        // The CREATE TABLE is the v1 (bitemporal) shape for fresh dbs; on an existing v0 db
        // it is a no-op and migrate() adds the new columns. Only columns present in BOTH v0
        // and v1 may be indexed here; the new-column indexes are created in migrate().
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA busy_timeout=5000;
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
                system_to_ms   INTEGER,
                scope          TEXT NOT NULL DEFAULT ''
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
            CREATE TABLE IF NOT EXISTS edges (
                from_uri   TEXT NOT NULL,
                to_uri     TEXT NOT NULL,
                rel        TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                PRIMARY KEY (from_uri, to_uri, rel)
            );
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_uri);
            "#,
        )
        .context("init schema")?;
        let store = Self { conn, read_scopes: None, agent_view: None };
        store.migrate().context("migrate schema to bitemporal (v1)")?;
        store.ensure_scope_column().context("add scope column (scope primitive)")?;
        Ok(store)
    }

    /// Scope primitive (daimon-docs: dm-lite/scope-primitive.md): one audience label per record version;
    /// '' = tenant-global, so every pre-scope row reads as global - truthful history.
    /// UNCONDITIONAL and idempotent, deliberately OUTSIDE the user_version-gated migrate():
    /// a v1 (bitemporal) database returns early from migrate(), which is exactly where the
    /// production db lives - the 0.3.2 release-gate migration test on a copy of it caught the
    /// scope ALTER never running when it sat inside that block.
    fn ensure_scope_column(&self) -> Result<()> {
        let missing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name='scope'",
            [],
            |r| r.get(0),
        )?;
        if missing == 0 {
            self.conn.execute_batch("ALTER TABLE entries ADD COLUMN scope TEXT NOT NULL DEFAULT ''")?;
        }
        Ok(())
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
        // BEGIN IMMEDIATE takes the write lock up front (with busy_timeout, a concurrent
        // open waits rather than failing with SQLITE_BUSY mid-migration).
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
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
            // New-column indexes (safe now that the columns exist). idx_entries_sysfrom serves
            // latest_save_ms's MAX() as an O(1) seek - it runs on every prompt and the table is
            // append-only, so an unindexed scan grows without bound.
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_entries_uri     ON entries(uri);
                 CREATE INDEX IF NOT EXISTS idx_entries_syscur  ON entries(system_to_ms);
                 CREATE INDEX IF NOT EXISTS idx_entries_sysfrom ON entries(system_from_ms);",
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
        let kind = Kind::from_str(&kind_s).unwrap_or_else(|| {
            // A stored kind we do not recognize (a forward-version record) is read back as Memory
            // so the row is never lost, but warn once: a future persona/protocol kind read as
            // Memory would silently drop out of the persona() boot layer.
            warn_unknown_kind_once(&kind_s);
            Kind::Memory
        });
        Ok(Entry {
            uri: row.get("uri")?,
            kind,
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
            scope: row.get("scope")?,
        })
    }

    /// Restrict reads to `scopes` (plus global `''`, which every reader sees). None = full
    /// tenant. Set once at open (local `DM_SCOPE`) or per request under the tenant lock
    /// (server, from the token) - the scope primitive's read half.
    pub fn set_read_scopes(&mut self, scopes: Option<Vec<String>>) {
        self.read_scopes = scopes.map(|v| v.into_iter().collect());
    }

    /// Restrict reads to one agent's view of the `agents/` tree (None = no restriction).
    pub fn set_agent_view(&mut self, agent: Option<String>) {
        self.agent_view = agent;
    }

    /// Is a record with this scope visible to the current reader?
    fn scope_ok(&self, scope: &str) -> bool {
        match &self.read_scopes {
            None => true,
            Some(set) => scope.is_empty() || set.contains(scope),
        }
    }

    /// Is a record in this namespace visible to the current agent identity? Everything
    /// outside `agents/` is; inside it, only the caller's own subtree. The prefix and the
    /// agent segment compare CASE-INSENSITIVELY (2nd-opinion follow-up: `Agents/shesta`
    /// must not slip past a guard that only knows `agents/`); agent labels themselves are
    /// canonical lowercase, so the caller side is already normalized.
    fn ns_ok(&self, namespace: &str) -> bool {
        let Some(a) = &self.agent_view else { return true };
        match crate::entry::split_agents_tree(namespace) {
            None => true,
            Some(owner) => owner.eq_ignore_ascii_case(a),
        }
    }

    /// The combined per-row read gate: scope (audience) AND agent tree.
    fn row_ok(&self, e: &Entry) -> bool {
        self.scope_ok(&e.scope) && self.ns_ok(&e.namespace)
    }

    /// Over-fetch when EITHER filter is active (both are post-query).
    fn filtered_reads(&self) -> bool {
        self.read_scopes.is_some() || self.agent_view.is_some()
    }

    /// Over-fetch factor for scope-filtered reads: the SQL LIMIT counts rows BEFORE the
    /// post-query scope filter, so a scoped reader fetches deeper to keep `limit` visible
    /// results. Unscoped readers pay nothing.
    fn fetch_depth(&self, limit: usize) -> usize {
        if self.filtered_reads() {
            limit.saturating_mul(4)
        } else {
            limit
        }
    }

    /// Fetch the current (live) entry for a uri (used by RRF fusion to hydrate vector hits).
    /// Scope-gated: an out-of-scope record reads as absent - this is what keeps invisible
    /// records out of rider hydration for free.
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub fn get(&self, uri: &str) -> Result<Option<Entry>> {
        let now = crate::entry::now_ms();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND {CURRENT} ORDER BY system_from_ms DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![uri, now, now])?;
        if let Some(r) = rows.next()? {
            let e = Self::row_to_entry(r)?;
            Ok(if self.row_ok(&e) { Some(e) } else { None })
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

    /// Bump many access signals in ONE transaction: recall calls this per hit set, and one
    /// commit instead of `limit` autocommits keeps the read path to a single WAL fsync.
    pub fn bump_signals(&self, uris: &[&str], now_ms: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO signals(uri, access_count, last_access_ms) VALUES(?1, 1, ?2) \
                 ON CONFLICT(uri) DO UPDATE SET access_count = access_count + 1, last_access_ms = ?2",
            )?;
            for uri in uris {
                stmt.execute(params![uri, now_ms])?;
            }
        }
        tx.commit()?;
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

/// Upper bound on rows scanned by an as-of query before keyword filtering, so a tenant with a
/// large history cannot make `recall_as_of` pull the whole table into memory. Rows are ordered
/// importance-first, so the cap keeps the most significant slice.
const MAX_AS_OF_SCAN: usize = 10_000;

/// Warn at most once per process that a stored kind was not recognized (read back as Memory).
fn warn_unknown_kind_once(kind: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("dmem: stored record kind '{kind}' is not recognized; reading it as 'memory' (newer dmem?)");
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

/// Sentinel for an open-ended (unbounded) valid_to in interval math: `None` reads as +infinity.
const VALID_OPEN: i64 = i64::MAX;
fn vto(v: Option<i64>) -> i64 {
    v.unwrap_or(VALID_OPEN)
}
/// Do the half-open valid intervals [af, at) and [bf, bt) overlap? (None = open / +infinity.)
fn intervals_overlap(af: i64, at: Option<i64>, bf: i64, bt: Option<i64>) -> bool {
    af < vto(bt) && bf < vto(at)
}

impl SqliteStore {
    /// All system-current versions (id + entry) for a dedup_key. Bitemporal: an entity can have
    /// several at once, partitioning its valid-time line into non-overlapping segments.
    /// Current live rows for a dedup key WITHIN ONE SCOPE. Dedup/supersede is per-scope by
    /// design (scope-primitive doc, section 4): the same title in two scopes is two records
    /// on two sides of a fence - they must never supersede each other across it.
    fn current_rows(conn: &Connection, dedup_key: &str, scope: &str) -> Result<Vec<(i64, Entry)>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, {COLS} FROM entries WHERE dedup_key=?1 AND scope=?2 AND system_to_ms IS NULL"
        ))?;
        let rows = stmt
            .query_map(params![dedup_key, scope], |row| Ok((row.get::<_, i64>("id")?, Self::row_to_entry(row)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// As `current_rows`, keyed by uri (used by `invalidate`).
    fn current_rows_by_uri(conn: &Connection, uri: &str) -> Result<Vec<(i64, Entry)>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, {COLS} FROM entries WHERE uri=?1 AND system_to_ms IS NULL"
        ))?;
        let rows = stmt
            .query_map(params![uri], |row| Ok((row.get::<_, i64>("id")?, Self::row_to_entry(row)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// System-close one version (drop it from current belief) and remove its FTS row. The row is
    /// retained (append-only); only `system_to_ms` is stamped.
    fn close_version(conn: &Connection, id: i64, now: i64) -> Result<()> {
        conn.execute("UPDATE entries SET system_to_ms=?1 WHERE id=?2", params![now, id])?;
        conn.execute("DELETE FROM entries_fts WHERE idref=?1", params![id])?;
        Ok(())
    }

    /// Insert one new system-current version (system_from=now, system_to=NULL) + its FTS row.
    /// Carries the entry's valid interval verbatim, so remainders preserve the prior body.
    fn insert_version(conn: &Connection, e: &Entry, now: i64) -> Result<()> {
        let tags = serde_json::to_string(&e.tags).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,\
             created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms,scope) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,?13)",
            params![
                e.uri, e.kind.as_str(), e.namespace, e.title, e.body, tags,
                e.importance, e.dedup_key, e.created_ms, e.valid_from_ms, e.valid_to_ms, now, e.scope
            ],
        )?;
        let new_id = conn.last_insert_rowid();
        let fts_text = format!("{} {} {}", e.title, e.body, e.tags.join(" "));
        conn.execute("INSERT INTO entries_fts(idref, text) VALUES (?1, ?2)", params![new_id, fts_text])?;
        Ok(())
    }
}

impl MemoryStore for SqliteStore {
    /// Bitemporal upsert with valid-time splitting. The new entry asserts its body over its valid
    /// interval [valid_from, valid_to). Any system-current segment of the same entity that OVERLAPS
    /// that interval is system-closed and its non-overlapping remainder(s) re-inserted (carrying
    /// the old body), then the new segment is inserted. Non-overlapping segments are untouched, so
    /// the entity can hold several valid-time segments at once. Whole thing is one transaction.
    ///
    /// - new interval == an existing segment  -> pure correction (close old, no remainder).
    /// - new interval is a sub-range          -> the world changed (one or two remainders kept).
    /// - identical body already covering it    -> no-op (no version churn on re-imports).
    fn put(&self, e: &Entry) -> Result<()> {
        // Reject an inverted or zero-width valid interval: it would otherwise split a segment into
        // overlapping remainders, breaking the non-overlapping-current-segments invariant. This is
        // the single chokepoint every write path funnels through (CLI / HTTP / MCP).
        if let Some(vt) = e.valid_to_ms {
            if vt <= e.valid_from_ms {
                anyhow::bail!("valid_to ({}) must be greater than valid_from ({})", vt, e.valid_from_ms);
            }
        }
        let now = crate::entry::now_ms();
        let tx = self.conn.unchecked_transaction()?;
        let current = Self::current_rows(&tx, &e.dedup_key, &e.scope)?;
        // Idempotent: a current segment with the SAME body that already covers the new interval
        // means we already believe this -> nothing to record.
        let already = current.iter().any(|(_, r)| {
            r.body == e.body && r.valid_from_ms <= e.valid_from_ms && vto(r.valid_to_ms) >= vto(e.valid_to_ms)
        });
        if already {
            tx.commit()?;
            return Ok(());
        }
        for (id, r) in &current {
            if !intervals_overlap(r.valid_from_ms, r.valid_to_ms, e.valid_from_ms, e.valid_to_ms) {
                continue;
            }
            Self::close_version(&tx, *id, now)?;
            // left remainder [r.valid_from, e.valid_from)
            if r.valid_from_ms < e.valid_from_ms {
                let mut left = r.clone();
                left.valid_to_ms = Some(e.valid_from_ms);
                Self::insert_version(&tx, &left, now)?;
            }
            // right remainder [e.valid_to, r.valid_to) - only if the new interval is bounded and
            // ends before this segment did.
            if let Some(evt) = e.valid_to_ms {
                if evt < vto(r.valid_to_ms) {
                    let mut right = r.clone();
                    right.valid_from_ms = evt;
                    Self::insert_version(&tx, &right, now)?;
                }
            }
        }
        Self::insert_version(&tx, e, now)?;
        tx.commit()?;
        Ok(())
    }

    fn recall_scored(&self, query: &str, limit: usize) -> Result<Vec<(Entry, f64)>> {
        let fq = match fts_query(query) {
            // No FTS terms (empty/short query) -> the recent() boot slice, which has no keyword
            // magnitude; tag each with INFINITY so the recall floor never gates the boot rows out.
            None => return Ok(self.recent(limit)?.into_iter().map(|e| (e, f64::INFINITY)).collect()),
            Some(q) => q,
        };
        let now = crate::entry::now_ms();
        // The canonical recall query (the trait's `recall` derives from this by dropping the score).
        // Filter to the current slice in SQL (JOIN entries) BEFORE LIMIT, so the limit counts only
        // live results - a stale FTS row can't consume a slot and crowd out a real match. ORDER BY
        // entries_fts.rank keeps positional rescoring aligned. SQLite bm25 (entries_fts.rank) is
        // NEGATIVE with more-negative = better, so return -rank (>= 0, larger = better).
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, entries_fts.rank AS kw_rank FROM entries_fts JOIN entries e ON e.id = entries_fts.idref \
             WHERE entries_fts MATCH ?1 AND e.system_to_ms IS NULL \
             AND e.valid_from_ms <= ?2 AND (e.valid_to_ms IS NULL OR e.valid_to_ms > ?2) \
             AND e.kind <> 'skill' \
             ORDER BY entries_fts.rank LIMIT ?3",
            COLS.split(',').map(|c| format!("e.{c}")).collect::<Vec<_>>().join(",")
        ))?;
        let out = stmt
            .query_map(params![fq, now, self.fetch_depth(limit) as i64], |r| {
                let e = Self::row_to_entry(r)?;
                let rank: f64 = r.get("kw_rank")?;
                Ok((e, -rank))
            })?
            .filter_map(|r| r.ok())
            .filter(|(e, _)| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(out)
    }

    fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        let now = crate::entry::now_ms();
        // Exclude skills: they are knowledge surfaced via the ~/.claude/skills projection, not the
        // recent/recall pool, so their full bodies never bloat the boot/per-prompt context.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {CURRENT} AND kind <> 'skill' \
             ORDER BY importance DESC, created_ms DESC LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![now, now, self.fetch_depth(limit) as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
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
            .query_map(params![kind, now, now, self.fetch_depth(limit) as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(rows)
    }

    fn by_kind_for_agent(&self, kind: &str, agent: Option<&str>, limit: usize) -> Result<Vec<Entry>> {
        // Canonicalize defensively so the namespace patterns below always see the same agent
        // spelling the token layer minted ([a-z0-9_-]); an empty/invalid label degrades to the
        // legacy full set rather than silently matching nothing.
        let agent = match agent.and_then(crate::config::canonical_agent) {
            Some(a) => a,
            None => return self.by_kind(kind, limit),
        };
        let now = crate::entry::now_ms();
        // GLOB, not LIKE: `_` is a LIKE wildcard but a literal in GLOB, and agent labels may
        // contain it. The canonical agent alphabet has no GLOB metacharacters (*?[]), so the
        // patterns are safe by construction.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE kind=?1 AND {CURRENT} \
             AND (namespace NOT GLOB 'agents/*' OR namespace = ?4 OR namespace GLOB ?5) \
             ORDER BY importance DESC, created_ms DESC LIMIT ?6"
        ))?;
        let own = format!("agents/{agent}");
        let own_tree = format!("agents/{agent}/*");
        let rows = stmt
            .query_map(params![kind, now, now, own, own_tree, self.fetch_depth(limit) as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(rows)
    }

    fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>> {
        // As-of can't use the FTS index (it holds only the current version of each record),
        // so scan `entries` for the as-of slice and keyword-filter in Rust. History is small
        // per tenant, so a linear scan is fine; this keeps as-of deterministic and simple.
        let pred = "system_from_ms <= ?1 AND (system_to_ms IS NULL OR system_to_ms > ?1) \
                    AND valid_from_ms <= ?2 AND (valid_to_ms IS NULL OR valid_to_ms > ?2) \
                    AND kind <> 'skill'";
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {pred} ORDER BY importance DESC, created_ms DESC"
        ))?;
        let all: Vec<Entry> = stmt
            .query_map(params![as_of_ms, valid_ms], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(MAX_AS_OF_SCAN)
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
        // History is content (scope decision Q2): each version is gated by its own scope, the
        // same rule as recall - anyone holding the scope may read the lineage, nobody else.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 ORDER BY system_from_ms DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![uri, self.fetch_depth(limit) as i64], Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(rows)
    }

    fn forget(&self, uri: &str) -> Result<usize> {
        let now = crate::entry::now_ms();
        let tx = self.conn.unchecked_transaction()?;
        let mut sel = tx.prepare("SELECT id FROM entries WHERE uri=?1 AND system_to_ms IS NULL")?;
        let ids: Vec<i64> = sel
            .query_map(params![uri], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(sel);
        for id in &ids {
            tx.execute("UPDATE entries SET system_to_ms=?1 WHERE id=?2", params![now, id])?;
            tx.execute("DELETE FROM entries_fts WHERE idref=?1", params![id])?;
        }
        // Cascade the graph layer: a retracted record's edges go with it (edges are curated
        // relations, not bitemporal facts - re-deriving them is safe, and dangling edges rot
        // the graph: they keep bridging BFS expansion through a node that no longer exists).
        // Only when the forget actually retracted something; `invalidate` deliberately does
        // NOT cascade - an invalidated record is still a live node of the belief history.
        if !ids.is_empty() {
            tx.execute("DELETE FROM edges WHERE from_uri=?1 OR to_uri=?1", params![uri])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    fn latest_save_ms(&self) -> Result<Option<i64>> {
        // MAX over ALL rows: every put inserts a row with system_from_ms = now, so this is the
        // wall-clock of the last write even if that version was later superseded or forgotten.
        // `MAX(...)` over an empty table yields one NULL row -> None.
        Ok(self
            .conn
            .query_row("SELECT MAX(system_from_ms) FROM entries", [], |r| r.get::<_, Option<i64>>(0))?)
    }

    fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        if valid_to_ms <= 0 {
            anyhow::bail!("invalidate valid_to must be a positive epoch-ms");
        }
        let now = crate::entry::now_ms();
        let tx = self.conn.unchecked_transaction()?;
        let mut affected = 0usize;
        for (id, r) in Self::current_rows_by_uri(&tx, uri)? {
            // segments that already end at/before the cut keep their full validity (untouched)
            if vto(r.valid_to_ms) <= valid_to_ms {
                continue;
            }
            // r extends past the cut -> close it; keep only the part before the cut, if any
            // (a segment entirely at/after the cut is dropped with no remainder).
            Self::close_version(&tx, id, now)?;
            if r.valid_from_ms < valid_to_ms {
                let mut keep = r.clone();
                keep.valid_to_ms = Some(valid_to_ms);
                Self::insert_version(&tx, &keep, now)?;
            }
            affected += 1;
        }
        tx.commit()?;
        Ok(affected)
    }

    fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO edges(from_uri, to_uri, rel, created_ms) VALUES(?1,?2,?3,?4)",
            params![from_uri, to_uri, rel, crate::entry::now_ms()],
        )?;
        Ok(())
    }

    fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM edges WHERE from_uri=?1 AND to_uri=?2 AND rel=?3",
            params![from_uri, to_uri, rel],
        )?)
    }

    fn edges_of(&self, uri: &str) -> Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_uri, to_uri, rel FROM edges WHERE from_uri=?1 OR to_uri=?1 ORDER BY created_ms",
        )?;
        let rows: Vec<Edge> = stmt
            .query_map(params![uri], |r| Ok(Edge { from_uri: r.get(0)?, to_uri: r.get(1)?, rel: r.get(2)? }))?
            .filter_map(|r| r.ok())
            .collect();
        if !self.filtered_reads() {
            return Ok(rows);
        }
        // Same reasoning as all_edges (2nd-opinion follow-up: this surface was missed): an
        // invisible record's uri slug is a title in disguise, and agents/<other> uris are
        // predictable - a restricted reader probing /edges must not enumerate them. The
        // queried uri itself being invisible reads as "no edges", matching get()'s absence.
        if !self.scope_visible(uri)? {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for e in rows {
            let other = if e.from_uri == uri { &e.to_uri } else { &e.from_uri };
            if self.scope_visible(other)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn neighbors(&self, seeds: &[String], depth: usize, limit: usize) -> Result<Vec<String>> {
        if seeds.is_empty() || depth == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        // Bounded breadth-first walk over the undirected edge set, capped by depth and limit. Done
        // in Rust rather than a recursive CTE so the dynamic seed set and caps stay simple and the
        // graph can never run away.
        use std::collections::HashSet;
        let mut visited: HashSet<String> = seeds.iter().cloned().collect();
        let mut frontier: Vec<String> = seeds.to_vec();
        let mut out: Vec<String> = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT to_uri FROM edges WHERE from_uri=?1 UNION SELECT from_uri FROM edges WHERE to_uri=?1",
        )?;
        for _ in 0..depth {
            if out.len() >= limit {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            'frontier: for u in &frontier {
                let hits: Vec<String> =
                    stmt.query_map(params![u], |r| r.get::<_, String>(0))?.filter_map(|x| x.ok()).collect();
                for h in hits {
                    if visited.insert(h.clone()) {
                        // scope gate at traversal, same rule as neighbors_scored: an invisible
                        // node neither appears nor bridges
                        if !self.scope_visible(&h)? {
                            continue;
                        }
                        next.push(h.clone());
                        out.push(h);
                        if out.len() >= limit {
                            break 'frontier;
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(out)
    }

    fn scope_visible(&self, uri: &str) -> Result<bool> {
        if !self.filtered_reads() {
            return Ok(true);
        }
        // The newest system-open version decides, on BOTH gates (scope + agent tree). A uri
        // with no live version stays "visible": dead nodes are the pruner's business, not
        // this gate's - hiding them here would silently change pre-scope bridging behavior.
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT scope, namespace FROM entries WHERE uri=?1 AND system_to_ms IS NULL \
                 ORDER BY system_from_ms DESC LIMIT 1",
                params![uri],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        Ok(match row {
            Some((s, ns)) => self.scope_ok(&s) && self.ns_ok(&ns),
            None => true,
        })
    }

    fn prune_dangling_edges(&self) -> Result<usize> {
        // Liveness here is system-time only (a system-open row exists), NOT the bitemporal
        // CURRENT filter: an invalidated record is still a real node of the belief history
        // and must keep its edges - only forgotten/never-existed endpoints are dead.
        Ok(self.conn.execute(
            "DELETE FROM edges WHERE
               NOT EXISTS (SELECT 1 FROM entries WHERE uri = edges.from_uri AND system_to_ms IS NULL)
               OR NOT EXISTS (SELECT 1 FROM entries WHERE uri = edges.to_uri AND system_to_ms IS NULL)",
            [],
        )?)
    }

    fn all_edges(&self, limit: usize) -> Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_uri, to_uri, rel FROM edges ORDER BY created_ms DESC LIMIT ?1",
        )?;
        let rows: Vec<Edge> = stmt
            .query_map(params![limit as i64], |r| Ok(Edge { from_uri: r.get(0)?, to_uri: r.get(1)?, rel: r.get(2)? }))?
            .filter_map(|r| r.ok())
            .collect();
        if !self.filtered_reads() {
            return Ok(rows);
        }
        // Scoped reader: a uri's slug is a title in disguise, so an edge naming an invisible
        // record leaks it. Keep only edges whose BOTH endpoints are visible. Per-endpoint
        // lookups, memoized - scoped graph dumps are rare (adapters/viewers), correctness wins.
        let mut memo: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        let mut out = Vec::new();
        for e in rows {
            let mut vis = |u: &str| -> Result<bool> {
                if let Some(v) = memo.get(u) {
                    return Ok(*v);
                }
                let v = self.scope_visible(u)?;
                memo.insert(u.to_string(), v);
                Ok(v)
            };
            if vis(&e.from_uri)? && vis(&e.to_uri)? {
                out.push(e);
            }
        }
        Ok(out)
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

    /// Like `mk` but with an explicit valid interval (the caller-managed application-time axis).
    fn mk_valid(title: &str, body: &str, vf: i64, vt: Option<i64>) -> Entry {
        let mut e = mk(Kind::Memory, "ns", title, body);
        e.valid_from_ms = vf;
        e.valid_to_ms = vt;
        e
    }

    fn current_segments(s: &SqliteStore, uri: &str) -> Vec<(String, i64, Option<i64>)> {
        let mut st = s
            .conn
            .prepare("SELECT body, valid_from_ms, valid_to_ms FROM entries WHERE uri=?1 AND system_to_ms IS NULL ORDER BY valid_from_ms")
            .unwrap();
        st.query_map(params![uri], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect()
    }

    #[test]
    fn by_kind_for_agent_scopes_the_agents_tree() {
        let s = mem_store();
        s.put(&mk(Kind::Persona, "shared/governance", "House Rules", "shared house rules")).unwrap();
        s.put(&mk(Kind::Persona, "agent/persona", "Operator Persona", "legacy persona (outside agents/)")).unwrap();
        s.put(&mk(Kind::Persona, "agents/izu/persona", "Izu Persona", "I am Izu")).unwrap();
        s.put(&mk(Kind::Persona, "agents/shesta/persona", "Shesta Persona", "I am Shesta")).unwrap();
        s.put(&mk(Kind::Protocol, "agent/protocol", "Behavioral Discipline", "shared protocol")).unwrap();

        // agent identity: shared governance + OWN agents/ records, never another agent's
        let izu: Vec<String> = s.by_kind_for_agent("persona", Some("izu"), 10).unwrap().into_iter().map(|e| e.title).collect();
        assert!(izu.contains(&"House Rules".to_string()), "shared governance goes to every agent");
        assert!(izu.contains(&"Operator Persona".to_string()), "legacy records outside agents/ stay visible");
        assert!(izu.contains(&"Izu Persona".to_string()), "own persona included");
        assert!(!izu.contains(&"Shesta Persona".to_string()), "another agent's persona is excluded");

        // protocols are shared (none live under agents/ here)
        let proto = s.by_kind_for_agent("protocol", Some("izu"), 10).unwrap();
        assert_eq!(proto.len(), 1);

        // no agent identity: the legacy full set, byte-identical to by_kind
        let all = s.by_kind_for_agent("persona", None, 10).unwrap();
        assert_eq!(all.len(), s.by_kind("persona", 10).unwrap().len());
        assert_eq!(all.len(), 4, "agent-less sees every persona record");

        // an agent with a `_` in its label matches literally (GLOB, not LIKE: no wildcard bleed)
        s.put(&mk(Kind::Persona, "agents/devXin/persona", "Not Dev_in", "wildcard bait")).unwrap();
        s.put(&mk(Kind::Persona, "agents/dev_in/persona", "Dev_in Persona", "I am dev_in")).unwrap();
        let dev: Vec<String> = s.by_kind_for_agent("persona", Some("dev_in"), 10).unwrap().into_iter().map(|e| e.title).collect();
        assert!(dev.contains(&"Dev_in Persona".to_string()));
        assert!(!dev.contains(&"Not Dev_in".to_string()), "underscore must not act as a wildcard");
    }

    #[test]
    fn put_and_recall_roundtrip() {
        let s = mem_store();
        s.put(&mk(Kind::Decision, "resources/x", "Lock LanceDB substrate", "we chose LanceDB for v2 vector storage")).unwrap();
        s.put(&mk(Kind::AgentLesson, "agent/lessons", "AVX2 gate", "the embedder needs AVX2 cpu instructions")).unwrap();
        let hits = s.recall("lancedb substrate", 5).unwrap();
        assert!(hits.iter().any(|e| e.title.contains("LanceDB")), "should recall the LanceDB decision");
        let hits2 = s.recall("avx2", 5).unwrap();
        assert!(hits2.iter().any(|e| e.title == "AVX2 gate"));
    }

    #[test]
    fn future_valid_facts_are_not_current_but_are_as_of_visible() {
        let s = mem_store();
        let now = now_ms();
        let future = now + 30 * 24 * 3_600_000; // valid a month from now
        let e = mk_valid("Price change", "the price becomes X", future, None);
        s.put(&e).unwrap();
        // the current slice must NOT see it on any read path...
        assert!(s.get(&e.uri).unwrap().is_none(), "get() must exclude a future-valid fact");
        assert!(s.recall("price becomes", 5).unwrap().is_empty(), "recall must exclude it");
        assert!(s.recent(10).unwrap().is_empty(), "recent must exclude it");
        assert!(s.by_kind("memory", 10).unwrap().is_empty(), "by_kind must exclude it");
        // ...while the as-of slice at its validity start does (the two slices must agree)
        let asof = s.recall_as_of("price becomes", 5, now_ms(), future + 1).unwrap();
        assert!(asof.iter().any(|x| x.uri == e.uri), "as-of at validity must see it");
        // and a normally-saved (valid-from-now) record is still immediately visible
        let n = mk(Kind::Memory, "ns", "Normal fact", "saved and true right now");
        s.put(&n).unwrap();
        assert!(s.get(&n.uri).unwrap().is_some(), "present-valid record stays visible");
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
    fn recall_filters_valid_expired_before_limit() {
        let s = mem_store();
        let live = mk(Kind::Memory, "ns", "alpha live", "alpha token here");
        s.put(&live).unwrap();
        // hand-insert a system-current but valid-EXPIRED row (valid_to in the past) that also
        // matches "alpha"; it must not consume the LIMIT slot ahead of the live row.
        s.conn
            .execute(
                "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,\
                 created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms) \
                 VALUES('daimon://expired','memory','ns','alpha expired','alpha token here','[]',\
                 50,'daimon://expired',1000,1000,2000,1000,NULL)",
                [],
            )
            .unwrap();
        let id = s.conn.last_insert_rowid();
        s.conn
            .execute(
                "INSERT INTO entries_fts(idref, text) VALUES (?1, ?2)",
                params![id, "alpha expired alpha token here"],
            )
            .unwrap();
        let hits = s.recall("alpha", 1).unwrap();
        assert_eq!(hits.len(), 1, "limit 1 must return a LIVE row, not be spent on the expired one");
        assert_eq!(hits[0].uri, live.uri);
    }

    #[test]
    fn forget_drops_from_recall_but_keeps_history() {
        let s = mem_store();
        let e = mk(Kind::Memory, "ns", "secret note", "alpha bravo charlie");
        s.put(&e).unwrap();
        assert_eq!(s.recall("alpha", 5).unwrap().len(), 1);
        // forget closes the current version
        assert_eq!(s.forget(&e.uri).unwrap(), 1);
        assert!(s.recall("alpha", 5).unwrap().is_empty(), "forgotten record is gone from recall");
        assert!(s.recent(5).unwrap().is_empty(), "and from recent");
        // but the lineage is retained (append-only)
        assert_eq!(s.history(&e.uri, 5).unwrap().len(), 1, "history still holds the closed version");
        // forgetting again is a no-op (nothing current)
        assert_eq!(s.forget(&e.uri).unwrap(), 0);
    }

    #[test]
    fn latest_save_ms_tracks_newest_write_not_importance() {
        let s = mem_store();
        assert_eq!(s.latest_save_ms().unwrap(), None, "empty store -> None");
        // a high-importance record saved first, then a low-importance one. The newest SAVE wins,
        // even though recent() (importance-ordered) would surface the high-importance one.
        s.put(&mk(Kind::Persona, "agent/persona", "Persona", "I am Izu.")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.put(&mk(Kind::Memory, "ns", "a quick note", "body")).unwrap();
        let latest = s.latest_save_ms().unwrap().expect("some save");
        let newest_row: i64 = s
            .conn
            .query_row("SELECT MAX(system_from_ms) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(latest, newest_row, "latest_save_ms = MAX(system_from_ms)");
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

    // --- bitemporal valid-time (Option B) ---

    #[test]
    fn change_splits_valid_time_and_keeps_old_segment() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "role");
        s.put(&mk_valid("role", "architect", 100, None)).unwrap();
        s.put(&mk_valid("role", "lead", 200, None)).unwrap(); // the world changed at 200
        // current belief (valid now): lead
        let now = s.recall("role", 10).unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].body, "lead");
        // valid-as-of 150 (per current belief): architect
        let past = s.recall_as_of("role", 10, now_ms(), 150).unwrap();
        assert!(past.iter().any(|e| e.body == "architect"));
        assert!(!past.iter().any(|e| e.body == "lead"));
        // two system-current segments coexist
        assert_eq!(
            current_segments(&s, &uri),
            vec![("architect".into(), 100, Some(200)), ("lead".into(), 200, None)]
        );
    }

    #[test]
    fn correction_same_interval_replaces_without_remainder() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "wrong", 100, None)).unwrap();
        s.put(&mk_valid("k", "right", 100, None)).unwrap(); // same interval = pure correction
        assert_eq!(current_segments(&s, &uri), vec![("right".into(), 100, None)]);
        let total: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries WHERE uri=?1", params![uri], |r| r.get(0)).unwrap();
        assert_eq!(total, 2, "old belief retained in system time (append-only)");
    }

    #[test]
    fn identical_resave_is_a_noop() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "same", 100, None)).unwrap();
        s.put(&mk_valid("k", "same", 200, None)).unwrap(); // same body, already covered -> no-op
        let total: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries WHERE uri=?1", params![uri], |r| r.get(0)).unwrap();
        assert_eq!(total, 1, "no new version for an unchanged re-save");
    }

    #[test]
    fn invalidate_ends_validity_keeping_history() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "live", 100, None)).unwrap();
        assert_eq!(s.invalidate(&uri, 300).unwrap(), 1);
        assert!(s.recall("live", 5).unwrap().is_empty(), "no longer true now");
        let past = s.recall_as_of("live", 5, now_ms(), 200).unwrap();
        assert!(past.iter().any(|e| e.body == "live"), "valid-as-of 200 still sees it");
        assert_eq!(current_segments(&s, &uri), vec![("live".into(), 100, Some(300))]);
    }

    #[test]
    fn sub_interval_update_creates_two_remainders() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "base", 100, None)).unwrap();
        s.put(&mk_valid("k", "blip", 200, Some(300))).unwrap(); // a value true only for [200,300)
        assert_eq!(
            current_segments(&s, &uri),
            vec![
                ("base".into(), 100, Some(200)),
                ("blip".into(), 200, Some(300)),
                ("base".into(), 300, None),
            ]
        );
        assert_eq!(s.recall("base", 5).unwrap()[0].body, "base"); // valid now (>300)
        assert!(s.recall_as_of("blip", 5, now_ms(), 250).unwrap().iter().any(|e| e.body == "blip"));
    }

    #[test]
    fn put_rejects_inverted_or_zero_width_interval() {
        let s = mem_store();
        assert!(s.put(&mk_valid("k", "x", 200, Some(100))).is_err(), "inverted interval rejected");
        assert!(s.put(&mk_valid("k", "x", 100, Some(100))).is_err(), "zero-width interval rejected");
        assert!(s.put(&mk_valid("k", "x", 100, Some(200))).is_ok(), "normal bounded interval accepted");
        assert!(s.put(&mk_valid("k", "y", 100, None)).is_ok(), "open interval accepted");
    }

    #[test]
    fn invalidate_rejects_nonpositive_cut() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "v", 100, None)).unwrap();
        assert!(s.invalidate(&uri, 0).is_err());
        assert!(s.invalidate(&uri, -5).is_err());
        assert!(s.invalidate(&uri, 300).is_ok());
    }

    #[test]
    fn backdated_superset_replaces_without_left_remainder() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "v1", 200, None)).unwrap(); // [200, inf)
        s.put(&mk_valid("k", "v2", 100, None)).unwrap(); // backdated, fully subsumes v1
        assert_eq!(current_segments(&s, &uri), vec![("v2".into(), 100, None)]);
    }

    #[test]
    fn idempotency_boundary_exact_vs_poke_past_end() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "a", 100, Some(300))).unwrap();
        // same body, interval fully inside the existing one -> already covered -> no-op
        s.put(&mk_valid("k", "a", 150, Some(250))).unwrap();
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries WHERE uri=?1", params![uri], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "covered same-body re-save creates no version");
        // poke past the end -> NOT covered -> extends to [100,400)
        s.put(&mk_valid("k", "a", 100, Some(400))).unwrap();
        assert_eq!(current_segments(&s, &uri), vec![("a".into(), 100, Some(400))]);
    }

    #[test]
    fn multi_segment_entity_keeps_fts_mirror_and_splits_further() {
        let s = mem_store();
        let uri = make_uri("ns", Kind::Memory, "k");
        s.put(&mk_valid("k", "base", 100, None)).unwrap();
        s.put(&mk_valid("k", "blip", 200, Some(300))).unwrap(); // -> base[100,200) blip[200,300) base[300,inf)
        assert_eq!(current_segments(&s, &uri).len(), 3);
        let fts_eq_current = |s: &SqliteStore| -> (i64, i64) {
            let fts: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries_fts", [], |r| r.get(0)).unwrap();
            let cur: i64 = s.conn.query_row("SELECT COUNT(*) FROM entries WHERE system_to_ms IS NULL", [], |r| r.get(0)).unwrap();
            (fts, cur)
        };
        let (f1, c1) = fts_eq_current(&s);
        assert_eq!(f1, c1, "FTS mirrors current rows");
        assert_eq!(f1, 3);
        // a change overlapping TWO existing segments (exercises the multi-row split loop)
        s.put(&mk_valid("k", "mid", 150, Some(250))).unwrap();
        assert_eq!(
            current_segments(&s, &uri),
            vec![
                ("base".into(), 100, Some(150)),
                ("mid".into(), 150, Some(250)),
                ("blip".into(), 250, Some(300)),
                ("base".into(), 300, None),
            ]
        );
        let (f2, c2) = fts_eq_current(&s);
        assert_eq!(f2, c2, "FTS still mirrors after a multi-segment split");
        assert_eq!(c2, 4);
    }

    // --- graph layer ---

    #[test]
    fn graph_link_traverse_unlink_and_resolve() {
        let s = mem_store();
        let a = mk(Kind::Decision, "resources/p", "Arch decision", "we chose X");
        let b = mk(Kind::ResourceSummary, "resources/p", "Project P", "the project");
        let c = mk(Kind::AgentLesson, "agent/lessons", "Lesson L", "learned Y");
        s.put(&a).unwrap();
        s.put(&b).unwrap();
        s.put(&c).unwrap();
        s.link(&a.uri, &b.uri, "part-of").unwrap();
        s.link(&a.uri, &c.uri, "sources").unwrap();
        s.link(&a.uri, &b.uri, "part-of").unwrap(); // idempotent
        assert_eq!(s.edges_of(&a.uri).unwrap().len(), 2);
        assert_eq!(s.edges_of(&b.uri).unwrap().len(), 1, "b sees the incoming a->b edge");
        // 1-hop neighbors of a = {b, c}
        let n1 = s.neighbors(&[a.uri.clone()], 1, 10).unwrap();
        assert_eq!(n1.len(), 2);
        assert!(n1.contains(&b.uri) && n1.contains(&c.uri));
        // undirected: 1-hop of b = {a}; 2-hop of b reaches c via a, excluding seed b
        assert_eq!(s.neighbors(&[b.uri.clone()], 1, 10).unwrap(), vec![a.uri.clone()]);
        let n2 = s.neighbors(&[b.uri.clone()], 2, 10).unwrap();
        assert!(n2.contains(&c.uri) && n2.contains(&a.uri) && !n2.contains(&b.uri));
        // unlink
        assert_eq!(s.unlink(&a.uri, &c.uri, "sources").unwrap(), 1);
        assert_eq!(s.edges_of(&a.uri).unwrap().len(), 1);
        assert_eq!(s.all_edges(100).unwrap().len(), 1);
    }

    #[test]
    fn neighbors_respects_limit_and_depth() {
        let s = mem_store();
        let hub = mk(Kind::Memory, "ns", "hub", "h");
        s.put(&hub).unwrap();
        for i in 0..5 {
            let sp = mk(Kind::Memory, "ns", &format!("spoke {i}"), "s");
            s.put(&sp).unwrap();
            s.link(&hub.uri, &sp.uri, "links").unwrap();
        }
        assert_eq!(s.neighbors(&[hub.uri.clone()], 1, 3).unwrap().len(), 3, "limit caps the neighborhood");
        assert!(s.neighbors(&[hub.uri.clone()], 0, 10).unwrap().is_empty(), "depth 0 -> nothing");
    }

    #[test]
    fn neighbors_scored_decays_per_hop_with_via_provenance() {
        // chain: a -> b -> c; seed a with weight 1.0, decay 0.5
        let s = mem_store();
        let a = mk(Kind::Memory, "ns", "a", "a");
        let b = mk(Kind::Memory, "ns", "b", "b");
        let c = mk(Kind::Memory, "ns", "c", "c");
        for e in [&a, &b, &c] {
            s.put(e).unwrap();
        }
        s.link(&a.uri, &b.uri, "links").unwrap();
        s.link(&b.uri, &c.uri, "informed").unwrap();
        let hits = s.neighbors_scored(&[(a.uri.clone(), 1.0)], 2, 10, 0.5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].uri.as_str(), hits[0].hop, hits[0].score), (b.uri.as_str(), 1, 0.5), "hop-1 first");
        assert_eq!(hits[0].via, a.uri, "b arrived via a");
        assert_eq!((hits[1].uri.as_str(), hits[1].hop, hits[1].score), (c.uri.as_str(), 2, 0.25), "hop-2 decays again");
        assert_eq!((hits[1].via.as_str(), hits[1].rel.as_str()), (b.uri.as_str(), "informed"), "c arrived via b");
    }

    #[test]
    fn neighbors_scored_best_score_wins_across_seeds() {
        // m is adjacent to both a strong seed (1.0) and a weak one (0.2): keep the strong path.
        let s = mem_store();
        let strong = mk(Kind::Memory, "ns", "strong", "s");
        let weak = mk(Kind::Memory, "ns", "weak", "w");
        let m = mk(Kind::Memory, "ns", "m", "m");
        for e in [&strong, &weak, &m] {
            s.put(e).unwrap();
        }
        s.link(&weak.uri, &m.uri, "links").unwrap(); // weak edge inserted first
        s.link(&strong.uri, &m.uri, "links").unwrap();
        let hits = s
            .neighbors_scored(&[(weak.uri.clone(), 0.2), (strong.uri.clone(), 1.0)], 2, 10, 0.5)
            .unwrap();
        assert_eq!(hits.len(), 1, "seeds are never emitted");
        assert_eq!(hits[0].score, 0.5, "best-scoring arrival wins");
        assert_eq!(hits[0].via, strong.uri, "provenance follows the winning path");
    }

    #[test]
    fn neighbors_scored_respects_caps() {
        let s = mem_store();
        let hub = mk(Kind::Memory, "ns", "hub", "h");
        s.put(&hub).unwrap();
        for i in 0..5 {
            let sp = mk(Kind::Memory, "ns", &format!("spoke {i}"), "s");
            s.put(&sp).unwrap();
            s.link(&hub.uri, &sp.uri, "links").unwrap();
        }
        assert_eq!(s.neighbors_scored(&[(hub.uri.clone(), 1.0)], 1, 3, 0.5).unwrap().len(), 3, "limit caps nodes");
        assert!(s.neighbors_scored(&[(hub.uri.clone(), 1.0)], 0, 10, 0.5).unwrap().is_empty(), "depth 0 -> nothing");
        assert!(s.neighbors_scored(&[], 2, 10, 0.5).unwrap().is_empty(), "no seeds -> nothing");
    }

    #[test]
    fn forget_cascades_edges_but_keeps_neighbors_edges() {
        let s = mem_store();
        let a = mk(Kind::Memory, "ns", "a", "a");
        let b = mk(Kind::Memory, "ns", "b", "b");
        let c = mk(Kind::Memory, "ns", "c", "c");
        for e in [&a, &b, &c] {
            s.put(e).unwrap();
        }
        s.link(&a.uri, &b.uri, "links").unwrap();
        s.link(&b.uri, &c.uri, "links").unwrap();
        s.link(&a.uri, &c.uri, "informed").unwrap();
        assert_eq!(s.forget(&b.uri).unwrap(), 1);
        assert!(s.edges_of(&b.uri).unwrap().is_empty(), "the forgotten node's edges cascade away");
        let a_edges = s.edges_of(&a.uri).unwrap();
        assert_eq!(a_edges.len(), 1, "a keeps only its edge to the still-live c");
        assert_eq!(a_edges[0].to_uri, c.uri);
        // forgetting a uri with no live versions is a no-op for the graph too
        assert_eq!(s.forget(&b.uri).unwrap(), 0);
    }

    #[test]
    fn v1_db_without_scope_gains_the_column_on_open() {
        // Regression for the 0.3.2 release-gate catch: a bitemporal (user_version=1) db - i.e.
        // PRODUCTION - returns early from migrate(), so the scope ALTER must live OUTSIDE that
        // gate or existing deployments break on first read after upgrade.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmv1s-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE entries (id INTEGER PRIMARY KEY, uri TEXT NOT NULL, kind TEXT NOT NULL,
                    namespace TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
                    tags TEXT NOT NULL DEFAULT '[]', importance INTEGER NOT NULL DEFAULT 50,
                    dedup_key TEXT NOT NULL, created_ms INTEGER NOT NULL,
                    valid_from_ms INTEGER NOT NULL DEFAULT 0, valid_to_ms INTEGER,
                    system_from_ms INTEGER NOT NULL DEFAULT 0, system_to_ms INTEGER);
                 CREATE VIRTUAL TABLE entries_fts USING fts5(idref UNINDEXED, text);
                 INSERT INTO entries(uri,kind,namespace,title,body,dedup_key,created_ms,valid_from_ms,system_from_ms)
                    VALUES('daimon://ns/memory/pre-scope','memory','ns','pre scope','body','daimon://ns/memory/pre-scope',1,1,1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        let e = s.get("daimon://ns/memory/pre-scope").unwrap().expect("pre-scope row readable");
        assert_eq!(e.scope, "", "pre-scope rows read as tenant-global");
        assert!(!s.recent(5).unwrap().is_empty(), "reads work post-ALTER");
    }

    #[test]
    fn scope_partitions_dedup_and_gates_reads() {
        let mut s = mem_store();
        // Same title in two scopes shares a uri/dedup_key (scope is NOT part of the uri by
        // design) but must NOT supersede across the fence: dedup is per (scope, dedup_key).
        // KNOWN WRINKLE (flagged for phase-2 review): a full-tenant reader's get(uri) on such
        // a collision returns the newest of the two - identity is ambiguous across scopes.
        let g = mk(Kind::Memory, "ns", "same title", "global body");
        let mut p = mk(Kind::Memory, "ns", "same title", "private body");
        p.scope = "user:a".into();
        s.put(&g).unwrap();
        s.put(&p).unwrap();
        let live: Vec<Entry> =
            s.history(&g.uri, 10).unwrap().into_iter().filter(|e| e.system_to_ms.is_none()).collect();
        assert_eq!(live.len(), 2, "both scopes' records stay live - no cross-scope supersede: {live:?}");

        // reads narrow to global + granted scopes
        let q = mk(Kind::Memory, "ns", "quiet other record", "in another scope");
        let mut q = q;
        q.scope = "user:b".into();
        s.put(&q).unwrap();
        s.set_read_scopes(Some(vec!["user:a".to_string()]));
        let recent = s.recent(10).unwrap();
        assert!(recent.iter().any(|e| e.scope.is_empty()), "global rides along for every reader");
        assert!(recent.iter().any(|e| e.scope == "user:a"), "granted scope visible");
        assert!(recent.iter().all(|e| e.scope != "user:b"), "ungranted scope invisible: {recent:?}");
        assert!(s.get(&q.uri).unwrap().is_none(), "get() reads an out-of-scope record as absent");
        assert!(
            s.history(&q.uri, 10).unwrap().is_empty(),
            "history follows the scope filter (decision Q2: history is content)"
        );
        // and the keyword channel too
        let hits = s.recall("quiet other record", 10).unwrap();
        assert!(hits.is_empty(), "recall must not leak ungranted scopes: {hits:?}");
    }

    #[test]
    fn prune_dangling_edges_spares_invalidated_records() {
        let s = mem_store();
        let a = mk(Kind::Memory, "ns", "a", "a");
        let inv = mk(Kind::Memory, "ns", "inv", "was true once");
        for e in [&a, &inv] {
            s.put(e).unwrap();
        }
        s.link(&a.uri, &inv.uri, "links").unwrap();
        // an edge inserted directly (bypassing forget's cascade) to a never-existed target
        s.link(&a.uri, "daimon://ns/memory/never-existed", "links").unwrap();
        // invalidate STRICTLY AFTER valid_from: valid-time closes, system-time stays open ->
        // still a live graph node. (A cut AT valid_from leaves an empty remainder - no
        // system-open row - which the pruner rightly treats as retraction; a same-millisecond
        // now_ms() here made that fire flakily.)
        s.invalidate(&inv.uri, inv.valid_from_ms + 10).unwrap();
        assert_eq!(s.prune_dangling_edges().unwrap(), 1, "only the never-existed edge goes");
        let a_edges = s.edges_of(&a.uri).unwrap();
        assert_eq!(a_edges.len(), 1, "the invalidated record keeps its edge");
        assert_eq!(a_edges[0].to_uri, inv.uri);
    }
}
