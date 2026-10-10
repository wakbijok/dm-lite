//! SQLite implementation of MemoryStore. Canonical records in `entries`, append-only and
//! bitemporal: every save is a new version (prior version closed in system time, never
//! deleted), with independent valid-time (true-in-world) and system-time (recorded-at)
//! axes. Keyword recall via an FTS5 index that holds ONLY the current version of each
//! record. As-of queries reconstruct any past slice. See `Entry` for the temporal model.

use crate::entry::{Edge, Entry, Kind};
use crate::store::MemoryStore;
use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection};
use std::path::Path;

/// Full column list (read order matches `row_to_entry`, which reads by name).
const COLS: &str = "uri,kind,namespace,title,body,tags,importance,dedup_key,\
created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms,scope";

/// The "current slice" predicate: the currently-recorded version (system_to NULL) that is
/// still true-in-world at `now`, bound at the numbered placeholder `?{n}` (numbered so the
/// visibility fragment from `visibility_sql` can be spliced into the same query).
fn current_at(n: usize) -> String {
    format!("system_to_ms IS NULL AND valid_from_ms <= ?{n} AND (valid_to_ms IS NULL OR valid_to_ms > ?{n})")
}

/// The agent-tree gate (`SqliteStore::ns_ok`) as SQL over the namespace column `ns`: rows
/// outside the `agents/` tree, plus `agent`'s own subtree. The prefix and the owner segment
/// compare case-insensitively (ASCII, like `ns_ok`), and by plain substring equality - no
/// GLOB/LIKE pattern, so no character of the agent label can act as a wildcard. Binds the
/// own-root at `?{n}` and the own-subtree prefix at `?{n+1}`.
fn agent_tree_sql(ns: &str, n: usize, agent: &str) -> (String, [Value; 2]) {
    let own = format!("agents/{}", agent.to_ascii_lowercase());
    let own_tree = format!("{own}/");
    let m = n + 1;
    (
        format!(
            "(substr(lower({ns}), 1, 7) <> 'agents/' OR lower({ns}) = ?{n} \
             OR substr(lower({ns}), 1, length(?{m})) = ?{m})"
        ),
        [Value::Text(own), Value::Text(own_tree)],
    )
}

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
        let mut conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        // Every transaction on this connection starts with BEGIN IMMEDIATE (`unchecked_transaction`
        // takes the connection's default). Every one of them writes (put, forget, invalidate,
        // signal bumps). A DEFERRED one starts as a reader and upgrades on its first write; when
        // another connection (a second process on the same file, a replication sidecar) has
        // committed in between, that upgrade fails at once with "database is locked", and
        // busy_timeout cannot help, because waiting would not make the stale snapshot current.
        // IMMEDIATE takes the write lock at BEGIN, where busy_timeout does wait.
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
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

    /// Scope primitive: one audience label per record version;
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
    /// agent segment compare CASE-INSENSITIVELY (2nd-opinion follow-up: `Agents/analyst`
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

    /// Is EITHER reader gate active (scope set or agent view)?
    pub(crate) fn filtered_reads(&self) -> bool {
        self.read_scopes.is_some() || self.agent_view.is_some()
    }

    /// Are reads restricted to a scope set (a scoped or adapter identity)? An agent label alone
    /// does not count: it narrows the `agents/` tree, not the audiences.
    pub(crate) fn reads_restricted(&self) -> bool {
        self.read_scopes.is_some()
    }

    /// `row_ok` as SQL: the scope (audience) gate and the agent-tree gate as one WHERE
    /// fragment over columns prefixed `p` ("" or an alias such as "e."), with numbered
    /// placeholders from `?{first}`; returns the fragment and the values to bind in order
    /// ("1" and none for an unrestricted reader). Filtering in SQL makes LIMIT count VISIBLE
    /// rows only: a post-query filter over a fixed over-fetch can be exhausted by any amount of
    /// invisible churn (one uri with 800 versions in another scope hid its global row).
    fn visibility_sql(&self, p: &str, first: usize) -> (String, Vec<Value>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();
        let mut n = first;
        if let Some(set) = &self.read_scopes {
            // One bound JSON array, however many scopes: no SQL built from reader input.
            let mut scopes: Vec<&String> = set.iter().collect();
            scopes.sort();
            clauses.push(format!("({p}scope = '' OR {p}scope IN (SELECT value FROM json_each(?{n})))"));
            vals.push(Value::Text(serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into())));
            n += 1;
        }
        if let Some(a) = &self.agent_view {
            let (sql, v) = agent_tree_sql(&format!("{p}namespace"), n, a);
            clauses.push(sql);
            vals.extend(v);
        }
        if clauses.is_empty() {
            ("1".into(), vals)
        } else {
            (clauses.join(" AND "), vals)
        }
    }

    /// Fetch the current (live, valid-now) entry for a uri: the newest current row AMONG the
    /// rows this reader may see (graph riders, edge-endpoint checks). The same uri can be held
    /// by several scopes (the same kind/namespace/title saved on both sides of a fence), so the
    /// reader gates filter IN SQL before the newest row is picked: an invisible newer row (a
    /// room re-saving a global record's title) never hides a visible older one. An out-of-scope
    /// record reads as absent - this is what keeps invisible records out of rider hydration.
    pub fn get(&self, uri: &str) -> Result<Option<Entry>> {
        let now = crate::entry::now_ms();
        let (vis, vis_vals) = self.visibility_sql("", 3);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND {} AND {vis} ORDER BY system_from_ms DESC, id DESC LIMIT 1",
            current_at(2)
        ))?;
        let binds = [Value::Text(uri.to_string()), Value::Integer(now)].into_iter().chain(vis_vals);
        let mut rows = stmt.query(params_from_iter(binds))?;
        if let Some(r) = rows.next()? {
            let e = Self::row_to_entry(r)?;
            Ok(if self.row_ok(&e) { Some(e) } else { None })
        } else {
            Ok(None)
        }
    }

    /// The current entry of `uri` in exactly `scope`, if this reader may see it: hydration of a
    /// vector hit, which embeds one (uri, scope) record - the hit must come back as THAT
    /// record, never as another scope's row under the same uri.
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub(crate) fn get_in_scope(&self, uri: &str, scope: &str) -> Result<Option<Entry>> {
        if !self.scope_ok(scope) {
            return Ok(None);
        }
        let now = crate::entry::now_ms();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND scope=?2 AND {} ORDER BY system_from_ms DESC, id DESC LIMIT 1",
            current_at(3)
        ))?;
        let mut rows = stmt.query(params![uri, scope, now])?;
        if let Some(r) = rows.next()? {
            let e = Self::row_to_entry(r)?;
            Ok(if self.row_ok(&e) { Some(e) } else { None })
        } else {
            Ok(None)
        }
    }

    /// The scopes holding a live row of `uri`, sorted, WITHOUT the reader gates (index
    /// maintenance and vector-hit resolution; never returned to a caller as content).
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub(crate) fn live_scopes_of(&self, uri: &str) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT scope FROM entries WHERE uri=?1 AND system_to_ms IS NULL ORDER BY scope")?;
        let out = st.query_map(params![uri], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(out)
    }

    /// Uris with a row written, or closed, at or after `since_ms` (system time): the records a
    /// period without a vector index may have left without a current vector.
    pub fn uris_changed_since(&self, since_ms: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT uri FROM entries WHERE system_from_ms >= ?1 OR system_to_ms >= ?1 ORDER BY uri",
        )?;
        let out = st.query_map([since_ms], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(out)
    }

    /// Every uri a non-global scope has ever held a row of, live or closed (the vector-slot
    /// migration's work list: exactly the uris whose one pre-slot vector may hold a scoped
    /// record's text).
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub(crate) fn uris_with_scoped_rows(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT uri FROM entries WHERE scope <> '' ORDER BY uri")?;
        let out = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(out)
    }

    /// The live rows a retracting mutation bounded to `scope` would select (None = every
    /// scope; Some(ws) = only rows in `ws`, the `forget_in`/`invalidate_in` selection): how
    /// many there are, and whether EVERY one passes the agent-tree gate. The mutation guard
    /// runs on this, not on reader visibility: a scoped writer may retract exactly its own
    /// rows of a uri, whatever other scopes hold under the same uri and whoever saved last.
    pub(crate) fn mutation_rows(&self, uri: &str, scope: Option<&str>) -> Result<(usize, bool)> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND (?2 IS NULL OR scope=?2)",
        )?;
        let namespaces: Vec<String> =
            stmt.query_map(params![uri, scope], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        let tree_ok = namespaces.iter().all(|ns| self.ns_ok(ns));
        Ok((namespaces.len(), tree_ok))
    }

    /// Does `ws` hold `uri` outright: at least one live row, and every live row in `ws`? The
    /// edge-ownership test (a write-restricted identity may add or remove an edge only when
    /// one endpoint is wholly its own). A uri that another scope, or global, also holds is
    /// shared, and its edges are every holder's. Judged on the stored rows WITHOUT the reader
    /// gates, like the mutation guard: an invisible holder still makes a uri shared.
    pub(crate) fn wholly_in_scope(&self, uri: &str, ws: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM entries WHERE uri=?1 AND system_to_ms IS NULL) \
             AND NOT EXISTS (SELECT 1 FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND scope <> ?2)",
            params![uri, ws],
            |r| r.get(0),
        )?)
    }

    /// The newest live (system-open) row of `uri`, in `scope` (None = any scope), WITHOUT the
    /// reader gates. Index maintenance only (the vector index must mirror what is stored,
    /// whoever the current reader is); never a read path.
    #[cfg_attr(not(feature = "zvec"), allow(dead_code))]
    pub(crate) fn newest_live(&self, uri: &str, scope: Option<&str>) -> Result<Option<Entry>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND (?2 IS NULL OR scope=?2) \
             ORDER BY system_from_ms DESC, id DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![uri, scope])?;
        match rows.next()? {
            Some(r) => Ok(Some(Self::row_to_entry(r)?)),
            None => Ok(None),
        }
    }

    /// Bump the runtime access signal under one key (`signal_key`; a bare uri is the global
    /// record's key, the only form before per-record signals).
    pub fn bump_signal(&self, uri: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO signals(uri, access_count, last_access_ms) VALUES(?1, 1, ?2) \
             ON CONFLICT(uri) DO UPDATE SET access_count = access_count + 1, last_access_ms = ?2",
            params![uri, now_ms],
        )?;
        Ok(())
    }

    /// Bump many access signals (by `signal_key`) in ONE transaction: recall calls this per hit
    /// set, and one commit instead of `limit` autocommits keeps the read path to a single WAL
    /// fsync.
    pub fn bump_signals(&self, uris: &[&str], now_ms: i64) -> Result<()> {
        // Nothing to bump (a recall with no hits): no transaction at all. Transactions start
        // IMMEDIATE on this connection, so an empty one would still wait for the write lock.
        if uris.is_empty() {
            return Ok(());
        }
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

    /// Read (access_count, last_access_ms) for a set of `signal_key`s (absent keys omitted).
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

#[cfg(test)]
impl SqliteStore {
    /// Test probe: the scopes holding a live row of `uri`, sorted (reader gates bypassed).
    pub(crate) fn live_scopes(&self, uri: &str) -> Vec<String> {
        self.live_scopes_of(uri).unwrap()
    }

    /// Test probe: every stored row of `uri`, live or closed, oldest first, as
    /// (scope, importance, created_ms) (reader gates bypassed).
    pub(crate) fn raw_rows(&self, uri: &str) -> Vec<(String, i64, i64)> {
        let mut st = self.conn.prepare("SELECT scope, importance, created_ms FROM entries WHERE uri=?1 ORDER BY id").unwrap();
        st.query_map(params![uri], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    /// Test probe: stored edges touching `uri` (reader gates bypassed).
    pub(crate) fn raw_edge_count(&self, uri: &str) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM edges WHERE from_uri=?1 OR to_uri=?1", params![uri], |r| r.get(0))
            .unwrap()
    }
}

/// Upper bound on rows scanned by an as-of query before keyword filtering, so a tenant with a
/// large history cannot make `recall_as_of` pull the whole table into memory. Rows are ordered
/// importance-first, so the cap keeps the most significant slice.
const MAX_AS_OF_SCAN: usize = 10_000;

/// Warn at most once per process that a stored kind was not recognized (read back as Memory).
/// The runtime-signal key of one RECORD, a (uri, scope) pair (the `signals` table's `uri`
/// column holds it). Since per-scope records, one uri can be several records - the global one
/// and room copies of the same title - and each ranks by its own access signal: a room that
/// keeps recalling its own copy must not raise the rank of the global record every other
/// audience sees (nor another room's copy). A GLOBAL record keeps the bare uri, the key every
/// signal was stored under before, so existing signal rows carry over unchanged with no
/// migration; a scoped record's key carries its scope, length-prefixed so the encoding is
/// injective (`s<len>:<scope><uri>`; a uri starts with `daimon://`, never like that).
pub(crate) fn signal_key(uri: &str, scope: &str) -> String {
    if scope.is_empty() {
        uri.to_string()
    } else {
        format!("s{}:{scope}{uri}", scope.len())
    }
}

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

    /// As `current_rows`, keyed by uri (used by `invalidate_in`). `scope` None = every scope's
    /// live rows; Some(s) = only the live rows in scope `s` (scope-exact mutation).
    fn current_rows_by_uri(conn: &Connection, uri: &str, scope: Option<&str>) -> Result<Vec<(i64, Entry)>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, {COLS} FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND (?2 IS NULL OR scope=?2)"
        ))?;
        let rows = stmt
            .query_map(params![uri, scope], |row| Ok((row.get::<_, i64>("id")?, Self::row_to_entry(row)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Does any live (system-open) row of `uri` remain, in ANY scope? The graph-cascade test:
    /// a uri's edges belong to the node, and the node exists while any audience still holds it.
    fn has_live_row(conn: &Connection, uri: &str) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM entries WHERE uri=?1 AND system_to_ms IS NULL)",
            params![uri],
            |r| r.get::<_, bool>(0),
        )?)
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
        let (vis, vis_vals) = self.visibility_sql("e.", 4);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, entries_fts.rank AS kw_rank FROM entries_fts JOIN entries e ON e.id = entries_fts.idref \
             WHERE entries_fts MATCH ?1 AND e.system_to_ms IS NULL \
             AND e.valid_from_ms <= ?2 AND (e.valid_to_ms IS NULL OR e.valid_to_ms > ?2) \
             AND e.kind <> 'skill' AND {vis} \
             ORDER BY entries_fts.rank LIMIT ?3",
            COLS.split(',').map(|c| format!("e.{c}")).collect::<Vec<_>>().join(",")
        ))?;
        let binds = [Value::Text(fq), Value::Integer(now), Value::Integer(limit as i64)].into_iter().chain(vis_vals);
        let out = stmt
            .query_map(params_from_iter(binds), |r| {
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
        let (vis, vis_vals) = self.visibility_sql("", 3);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {} AND kind <> 'skill' AND {vis} \
             ORDER BY importance DESC, created_ms DESC LIMIT ?2",
            current_at(1)
        ))?;
        let binds = [Value::Integer(now), Value::Integer(limit as i64)].into_iter().chain(vis_vals);
        let rows = stmt
            .query_map(params_from_iter(binds), Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(rows)
    }

    fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Entry>> {
        let now = crate::entry::now_ms();
        let (vis, vis_vals) = self.visibility_sql("", 4);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE kind=?1 AND {} AND {vis} \
             ORDER BY importance DESC, created_ms DESC LIMIT ?3",
            current_at(2)
        ))?;
        let binds =
            [Value::Text(kind.to_string()), Value::Integer(now), Value::Integer(limit as i64)].into_iter().chain(vis_vals);
        let rows = stmt
            .query_map(params_from_iter(binds), Self::row_to_entry)?
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
        // The agent's own tree by plain substring equality (no GLOB/LIKE wildcard to escape;
        // `_` stays literal) and case-insensitively, the same rule as the per-row agent gate:
        // `Agents/Analyst/...` is the analyst's tree, not shared pool.
        let (tree, tree_vals) = agent_tree_sql("namespace", 4, &agent);
        let (vis, vis_vals) = self.visibility_sql("", 6);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE kind=?1 AND {} AND {tree} AND {vis} \
             ORDER BY importance DESC, created_ms DESC LIMIT ?3",
            current_at(2)
        ))?;
        let binds = [Value::Text(kind.to_string()), Value::Integer(now), Value::Integer(limit as i64)]
            .into_iter()
            .chain(tree_vals)
            .chain(vis_vals);
        let rows = stmt
            .query_map(params_from_iter(binds), Self::row_to_entry)?
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
        // Reader gates in SQL too, so the MAX_AS_OF_SCAN cap counts visible rows only.
        let (vis, vis_vals) = self.visibility_sql("", 3);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE {pred} AND {vis} ORDER BY importance DESC, created_ms DESC"
        ))?;
        let binds = [Value::Integer(as_of_ms), Value::Integer(valid_ms)].into_iter().chain(vis_vals);
        let all: Vec<Entry> = stmt
            .query_map(params_from_iter(binds), Self::row_to_entry)?
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
        // Gated IN SQL, so LIMIT is exact however many invisible versions share the uri.
        let (vis, vis_vals) = self.visibility_sql("", 3);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM entries WHERE uri=?1 AND {vis} ORDER BY system_from_ms DESC, id DESC LIMIT ?2"
        ))?;
        let binds = [Value::Text(uri.to_string()), Value::Integer(limit as i64)].into_iter().chain(vis_vals);
        let rows = stmt
            .query_map(params_from_iter(binds), Self::row_to_entry)?
            .filter_map(|r| r.ok())
            .filter(|e| self.row_ok(e))
            .take(limit)
            .collect();
        Ok(rows)
    }

    fn forget_in(&self, uri: &str, scope: Option<&str>) -> Result<usize> {
        let now = crate::entry::now_ms();
        let tx = self.conn.unchecked_transaction()?;
        // Scope-exact selection: a scoped writer closes only its own scope's live rows; the
        // same uri held by another scope is another audience's record (same title on the
        // other side of the fence) and stays live. None = every scope (full-tenant writer).
        let mut sel = tx.prepare("SELECT id FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND (?2 IS NULL OR scope=?2)")?;
        let ids: Vec<i64> = sel
            .query_map(params![uri, scope], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(sel);
        for id in &ids {
            Self::close_version(&tx, *id, now)?;
        }
        // Cascade the graph layer: a retracted record's edges go with it (edges are curated
        // relations, not bitemporal facts - re-deriving them is safe, and dangling edges rot
        // the graph: they keep bridging BFS expansion through a node that no longer exists).
        // Only when the forget actually retracted something AND no live row of the uri is
        // left in any scope: while another audience still holds the uri, the node exists and
        // its edges are not this writer's to drop. `invalidate_in` cascades on the same test
        // only: a record whose cut leaves a live remainder is still a node of the belief history.
        if !ids.is_empty() && !Self::has_live_row(&tx, uri)? {
            tx.execute("DELETE FROM edges WHERE from_uri=?1 OR to_uri=?1", params![uri])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    fn latest_save_ms(&self) -> Result<Option<i64>> {
        // Newest over ALL rows the reader may see, live or closed: every put inserts a row with
        // system_from_ms = now, so this is the wall-clock of the last write even if that
        // version was later superseded or forgotten. Reader gates apply: a scoped reader must
        // not learn when a scope it cannot see writes (a timing side channel). ORDER BY ... LIMIT
        // 1 walks idx_entries_sysfrom from the top and stops at the first visible row (an
        // unrestricted reader's `1` makes that the MAX() seek it always was).
        let (vis, vis_vals) = self.visibility_sql("", 1);
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT system_from_ms FROM entries WHERE {vis} ORDER BY system_from_ms DESC LIMIT 1"))?;
        let mut rows = stmt.query(params_from_iter(vis_vals))?;
        Ok(match rows.next()? {
            Some(r) => r.get::<_, Option<i64>>(0)?,
            None => None,
        })
    }

    fn invalidate_in(&self, uri: &str, valid_to_ms: i64, scope: Option<&str>) -> Result<usize> {
        if valid_to_ms <= 0 {
            anyhow::bail!("invalidate valid_to must be a positive epoch-ms");
        }
        let now = crate::entry::now_ms();
        let tx = self.conn.unchecked_transaction()?;
        let mut affected = 0usize;
        for (id, r) in Self::current_rows_by_uri(&tx, uri, scope)? {
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
        // A cut at/before a segment's start drops it with no remainder. When that leaves the
        // uri with no live row in ANY scope, the node is gone exactly as after a forget, so its
        // edges go with it, in the same transaction (otherwise they dangle until the next
        // prune and keep naming a retracted record). A cut that leaves a live remainder, or
        // another scope's live row, keeps them: that node still exists.
        if affected > 0 && !Self::has_live_row(&tx, uri)? {
            tx.execute("DELETE FROM edges WHERE from_uri=?1 OR to_uri=?1", params![uri])?;
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
        // Any live row the reader may see makes the uri visible - the newest VISIBLE row
        // decides, so an invisible newer row (a room re-saving a global record's title) no
        // longer hides a visible older one. Filtered in SQL, on BOTH gates (scope + agent tree).
        let (vis, vis_vals) = self.visibility_sql("", 2);
        let visible_live: bool = self.conn.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM entries WHERE uri=?1 AND system_to_ms IS NULL AND {vis})"),
            params_from_iter([Value::Text(uri.to_string())].into_iter().chain(vis_vals)),
            |r| r.get(0),
        )?;
        if visible_live {
            return Ok(true);
        }
        // No visible live row. Live rows the reader cannot see keep the uri invisible; a uri
        // with no live row at all is judged by its NEWEST version ever (closed included): a
        // retracted room record's uri is a title in disguise and must stay hidden from other
        // rooms (and a dead agents/<b>/... uri from other agents) wherever a dangling edge
        // still names it. Only a uri with no row at all stays visible - the pre-scope
        // bridging behaviour for never-existed nodes (manual links to planned records).
        let row: Option<(String, String, bool)> = self
            .conn
            .query_row(
                "SELECT scope, namespace, system_to_ms IS NULL FROM entries WHERE uri=?1 \
                 ORDER BY (system_to_ms IS NULL) DESC, system_from_ms DESC, id DESC LIMIT 1",
                params![uri],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        Ok(match row {
            Some((_, _, true)) => false,
            Some((s, ns, false)) => self.scope_ok(&s) && self.ns_ok(&ns),
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
        s.put(&mk(Kind::Persona, "agents/assistant/persona", "Assistant Persona", "I am the assistant")).unwrap();
        s.put(&mk(Kind::Persona, "agents/analyst/persona", "Analyst Persona", "I am the analyst")).unwrap();
        s.put(&mk(Kind::Protocol, "agent/protocol", "Behavioral Discipline", "shared protocol")).unwrap();

        // agent identity: shared governance + OWN agents/ records, never another agent's
        let assistant: Vec<String> = s.by_kind_for_agent("persona", Some("assistant"), 10).unwrap().into_iter().map(|e| e.title).collect();
        assert!(assistant.contains(&"House Rules".to_string()), "shared governance goes to every agent");
        assert!(assistant.contains(&"Operator Persona".to_string()), "legacy records outside agents/ stay visible");
        assert!(assistant.contains(&"Assistant Persona".to_string()), "own persona included");
        assert!(!assistant.contains(&"Analyst Persona".to_string()), "another agent's persona is excluded");

        // protocols are shared (none live under agents/ here)
        let proto = s.by_kind_for_agent("protocol", Some("assistant"), 10).unwrap();
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
        s.put(&mk(Kind::Persona, "agent/persona", "Persona", "I am the assistant.")).unwrap();
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
    fn latest_save_ms_is_gated_by_the_readers_view() {
        // A scoped reader must not watch when a scope it cannot see writes (a timing side
        // channel): its latest save is the newest row it may read.
        let mut s = mem_store();
        s.put(&mk(Kind::Memory, "ns", "a global note", "body")).unwrap();
        let global_ms = s.latest_save_ms().unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut bob = mk(Kind::Memory, "ns", "bob private", "salary review");
        bob.scope = "user:bob".into();
        s.put(&bob).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.put(&mk(Kind::Persona, "agents/analyst", "Analyst persona", "I am the analyst")).unwrap();
        let newest = s.latest_save_ms().unwrap().unwrap();
        assert!(newest > global_ms, "the unrestricted reader sees every write");
        s.set_read_scopes(Some(vec!["room:a".into()]));
        s.set_agent_view(Some("assistant".into()));
        assert_eq!(s.latest_save_ms().unwrap(), Some(global_ms), "bob's write and another agent's tree stay unseen");
        s.set_agent_view(None);
        s.set_read_scopes(Some(vec!["user:bob".into()]));
        assert_eq!(s.latest_save_ms().unwrap(), Some(newest), "a reader of user:bob sees the newest visible write");
        let mut empty = mem_store();
        empty.set_read_scopes(Some(vec!["room:a".into()]));
        assert_eq!(empty.latest_save_ms().unwrap(), None, "nothing visible: None");
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
    fn signal_keys_are_per_record_and_global_keeps_the_bare_uri() {
        let uri = "daimon://resources/workspace/runbook/shared";
        assert_eq!(signal_key(uri, ""), uri, "a global record keeps the pre-0.3.7 key");
        assert_eq!(signal_key(uri, "room:a"), format!("s6:room:a{uri}"));
        assert_ne!(signal_key(uri, "room:a"), signal_key(uri, "room:b"));
        // injective: the scope's length is part of the key
        assert_ne!(signal_key("xdaimon://u", "ab"), signal_key("daimon://u", "abx"));
        // a signals row written by 0.3.6 (keyed by the bare uri) is the global record's
        let s = mem_store();
        s.bump_signal(uri, 1000).unwrap();
        s.bump_signals(&[&signal_key(uri, "room:a")], 2000).unwrap();
        let m = s.read_signals(&[signal_key(uri, ""), signal_key(uri, "room:a"), signal_key(uri, "room:b")]).unwrap();
        assert_eq!(m.get(uri).copied(), Some((1, 1000)));
        assert_eq!(m.get(&signal_key(uri, "room:a")).copied(), Some((1, 2000)));
        assert!(!m.contains_key(&signal_key(uri, "room:b")));
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

    // --- E1: scope-exact forget / invalidate ---

    /// The same record (kind/namespace/title, so the same uri) saved in `scope`.
    fn mk_in(scope: &str, body: &str) -> Entry {
        let mut e = mk(Kind::Runbook, "resources/workspace", "Shared runbook", body);
        e.scope = scope.into();
        e
    }

    /// A global anchor record linked FROM `uri` (so the uri has one edge).
    fn anchor_linked_from(s: &SqliteStore, uri: &str) -> String {
        let anchor = mk(Kind::Org, "resources/entities", "Anchor org", "a global anchor");
        s.put(&anchor).unwrap();
        s.link(uri, &anchor.uri, "about").unwrap();
        anchor.uri
    }

    #[test]
    fn scoped_forget_closes_only_its_own_scope_and_keeps_the_graph() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        s.put(&mk_in("room:b", "room b body")).unwrap();
        anchor_linked_from(&s, &g.uri);
        assert_eq!(s.live_scopes(&g.uri), vec!["", "room:a", "room:b"]);

        // room:a closes exactly its own row: the count covers room:a only (no count oracle)
        assert_eq!(s.forget_in(&g.uri, Some("room:a")).unwrap(), 1);
        assert_eq!(s.live_scopes(&g.uri), vec!["", "room:b"], "other scopes' rows stay live");
        assert_eq!(s.raw_edge_count(&g.uri), 1, "the node still exists elsewhere: edges stay");
        // nothing of room:a is left to close
        assert_eq!(s.forget_in(&g.uri, Some("room:a")).unwrap(), 0);
        // the lineage of the closed row is kept (append-only)
        let physical: i64 =
            s.conn.query_row("SELECT COUNT(*) FROM entries WHERE uri=?1", params![g.uri], |r| r.get(0)).unwrap();
        assert_eq!(physical, 3);
    }

    #[test]
    fn scoped_forget_of_the_last_live_row_cascades_edges() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        anchor_linked_from(&s, &g.uri);
        assert_eq!(s.forget_in(&g.uri, Some("")).unwrap(), 1, "a global-scoped selection closes the global row only");
        assert_eq!(s.raw_edge_count(&g.uri), 1, "room:a still holds the uri");
        assert_eq!(s.forget_in(&g.uri, Some("room:a")).unwrap(), 1);
        assert!(s.live_scopes(&g.uri).is_empty());
        assert_eq!(s.raw_edge_count(&g.uri), 0, "the last live row takes the edges with it");
    }

    #[test]
    fn full_tenant_forget_closes_every_scope_and_cascades() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        anchor_linked_from(&s, &g.uri);
        assert_eq!(s.forget_in(&g.uri, None).unwrap(), 2, "None = every scope, as before");
        assert!(s.live_scopes(&g.uri).is_empty());
        assert_eq!(s.raw_edge_count(&g.uri), 0);
        // the trait's plain forget is the full-tenant selection
        let h = mk(Kind::Memory, "ns", "plain", "p");
        s.put(&h).unwrap();
        let mut hs = h.clone();
        hs.scope = "room:a".into();
        s.put(&hs).unwrap();
        assert_eq!(s.forget(&h.uri).unwrap(), 2);
    }

    #[test]
    fn scoped_invalidate_cuts_only_its_own_scope() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        s.put(&mk_in("room:b", "room b body")).unwrap();
        anchor_linked_from(&s, &g.uri);
        let cut = now_ms() + 60_000; // a cut after valid_from leaves a live, bounded remainder
        assert_eq!(s.invalidate_in(&g.uri, cut, Some("room:a")).unwrap(), 1, "room:a's segment only");
        let rows: Vec<(String, Option<i64>)> = {
            let mut st = s
                .conn
                .prepare("SELECT scope, valid_to_ms FROM entries WHERE uri=?1 AND system_to_ms IS NULL ORDER BY scope")
                .unwrap();
            st.query_map(params![g.uri], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            rows,
            vec![("".into(), None), ("room:a".into(), Some(cut)), ("room:b".into(), None)],
            "only room:a's validity ends; the other scopes are untouched"
        );
        assert_eq!(s.raw_edge_count(&g.uri), 1, "invalidate keeps the edges of a live node");
        // no rows of its own in scope -> nothing affected
        assert_eq!(s.invalidate_in(&g.uri, cut, Some("room:c")).unwrap(), 0);
    }

    #[test]
    fn full_tenant_invalidate_cuts_every_scope() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        let cut = now_ms() + 60_000;
        assert_eq!(s.invalidate_in(&g.uri, cut, None).unwrap(), 2);
        assert_eq!(s.invalidate(&g.uri, cut - 1).unwrap(), 2, "the trait's plain invalidate is the full-tenant selection");
    }

    #[test]
    fn mutation_rows_counts_the_selection_and_checks_the_agent_tree() {
        let mut s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        assert_eq!(s.mutation_rows(&g.uri, None).unwrap(), (2, true));
        assert_eq!(s.mutation_rows(&g.uri, Some("room:a")).unwrap(), (1, true));
        assert_eq!(s.mutation_rows(&g.uri, Some("room:b")).unwrap(), (0, true));
        // a reader-scope binding does not narrow the mutation selection (it is not a read)
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert_eq!(s.mutation_rows(&g.uri, Some("room:a")).unwrap(), (1, true));
        // the agent-tree gate applies to every selected row
        let p = mk(Kind::Persona, "agents/analyst", "Analyst persona", "I am the analyst");
        s.put(&p).unwrap();
        s.set_agent_view(Some("assistant".into()));
        assert_eq!(s.mutation_rows(&p.uri, None).unwrap(), (1, false));
        s.set_agent_view(Some("analyst".into()));
        assert_eq!(s.mutation_rows(&p.uri, None).unwrap(), (1, true));
    }

    #[test]
    fn wholly_in_scope_needs_every_live_row_in_the_scope() {
        let mut s = mem_store();
        let e = mk_in("room:a", "room a body");
        s.put(&e).unwrap();
        assert!(s.wholly_in_scope(&e.uri, "room:a").unwrap(), "only room a holds it");
        assert!(!s.wholly_in_scope(&e.uri, "room:b").unwrap());
        assert!(!s.wholly_in_scope("daimon://never/existed", "room:a").unwrap(), "no live row: nobody owns it");
        // another scope holding the same uri makes it shared, whoever the reader is
        s.put(&mk_in("room:b", "room b body")).unwrap();
        s.set_read_scopes(Some(vec!["room:a".into()]));
        assert!(!s.wholly_in_scope(&e.uri, "room:a").unwrap(), "an invisible holder still shares it");
        // once room b's row is closed, room a owns it again; closed rows never count
        s.forget_in(&e.uri, Some("room:b")).unwrap();
        assert!(s.wholly_in_scope(&e.uri, "room:a").unwrap());
        // a global row is a holder like any other
        s.put(&mk_in("", "global body")).unwrap();
        assert!(!s.wholly_in_scope(&e.uri, "room:a").unwrap());
        assert!(!s.wholly_in_scope(&e.uri, "").unwrap());
    }

    // --- E2: dead-uri visibility and the invalidate cascade ---

    #[test]
    fn invalidate_that_leaves_no_live_row_cascades_edges() {
        // a room record linked to a global anchor, invalidated with valid_to BEFORE its start:
        // no remainder, no live row left -> the edge goes in the same transaction
        let mut s = mem_store();
        let r = mk_in("room:a", "room a body");
        s.put(&r).unwrap();
        let anchor = anchor_linked_from(&s, &r.uri);
        assert_eq!(s.invalidate_in(&r.uri, 1_000, Some("room:a")).unwrap(), 1);
        assert!(s.live_scopes(&r.uri).is_empty(), "a cut before the start leaves no remainder");
        assert_eq!(s.raw_edge_count(&r.uri), 0, "the edge is deleted");
        // and another room's view of the anchor never shows it
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert!(s.edges_of(&anchor).unwrap().is_empty());
    }

    #[test]
    fn invalidate_leaving_a_live_row_keeps_edges() {
        let s = mem_store();
        let g = mk_in("", "global body");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room a body")).unwrap();
        anchor_linked_from(&s, &g.uri);
        // room:a's row goes entirely, the global row is still live -> the node stays
        assert_eq!(s.invalidate_in(&g.uri, 1_000, Some("room:a")).unwrap(), 1);
        assert_eq!(s.live_scopes(&g.uri), vec![""]);
        assert_eq!(s.raw_edge_count(&g.uri), 1);
        // a cut after the start leaves a live (bounded) remainder -> the node stays too
        assert_eq!(s.invalidate_in(&g.uri, now_ms() + 60_000, None).unwrap(), 1);
        assert_eq!(s.raw_edge_count(&g.uri), 1);
        // the last live row cut before its start: now it cascades
        assert_eq!(s.invalidate_in(&g.uri, 1_000, None).unwrap(), 1);
        assert_eq!(s.raw_edge_count(&g.uri), 0);
    }

    #[test]
    fn dead_uri_visibility_follows_its_newest_row() {
        let mut s = mem_store();
        let r = mk_in("room:a", "room a body");
        s.put(&r).unwrap();
        s.forget_in(&r.uri, Some("room:a")).unwrap();
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert!(!s.scope_visible(&r.uri).unwrap(), "a dead room uri stays hidden from other rooms");
        s.set_read_scopes(Some(vec!["room:a".into()]));
        assert!(s.scope_visible(&r.uri).unwrap(), "and visible to its own room");
        s.set_read_scopes(None);
        assert!(s.scope_visible(&r.uri).unwrap(), "full tenant sees everything");

        // a dead GLOBAL uri is visible to every room (its newest row is global)
        let g = mk(Kind::Memory, "ns", "gone global", "g");
        s.put(&g).unwrap();
        s.forget(&g.uri).unwrap();
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert!(s.scope_visible(&g.uri).unwrap());

        // the NEWEST row ever decides: global first, then a room row, both closed -> the room
        let mut w = mk(Kind::Memory, "ns", "was global then room", "w");
        s.put(&w).unwrap();
        w.scope = "room:a".into();
        w.body = "room version".into();
        s.put(&w).unwrap();
        s.forget(&w.uri).unwrap();
        assert!(!s.scope_visible(&w.uri).unwrap(), "judged by the newest row ever (room:a)");

        // a uri with no rows at all stays visible (pre-scope bridging for never-existed nodes)
        assert!(s.scope_visible("daimon://ns/memory/never-existed").unwrap());
    }

    #[test]
    fn dead_uri_visibility_applies_the_agent_tree_gate() {
        let mut s = mem_store();
        let p = mk(Kind::Persona, "agents/analyst", "Analyst persona", "I am the analyst");
        s.put(&p).unwrap();
        s.forget(&p.uri).unwrap();
        s.set_agent_view(Some("assistant".into()));
        assert!(!s.scope_visible(&p.uri).unwrap(), "a dead foreign persona uri stays hidden");
        s.set_agent_view(Some("analyst".into()));
        assert!(s.scope_visible(&p.uri).unwrap());
    }

    #[test]
    fn dangling_edge_to_a_dead_room_uri_is_hidden_from_other_rooms() {
        // rot from before the cascade existed: an edge still names a retracted room record
        let mut s = mem_store();
        let anchor = mk(Kind::Org, "resources/entities", "Anchor org", "a global anchor");
        s.put(&anchor).unwrap();
        let far = mk(Kind::Memory, "ns", "far global", "reachable only through the dead node");
        s.put(&far).unwrap();
        let r = mk_in("room:a", "room a body");
        s.put(&r).unwrap();
        s.forget_in(&r.uri, Some("room:a")).unwrap();
        s.link(&r.uri, &anchor.uri, "about").unwrap(); // re-inserted after the cascade
        s.link(&r.uri, &far.uri, "links").unwrap();
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert!(s.edges_of(&anchor.uri).unwrap().is_empty(), "the dead room uri is not named");
        assert!(s.all_edges(100).unwrap().is_empty());
        assert!(
            s.neighbors(std::slice::from_ref(&anchor.uri), 3, 10).unwrap().is_empty(),
            "the dead room node neither appears nor bridges"
        );
        s.set_read_scopes(Some(vec!["room:a".into()]));
        assert_eq!(s.edges_of(&anchor.uri).unwrap().len(), 1, "its own room still sees the edge");
    }

    // --- E3: reader gates in SQL, so LIMIT is exact under invisible churn ---

    /// Append `n` closed versions of `e` in `scope` (plus one live one), newer than anything
    /// already stored - heavy invisible churn on one uri, written in one transaction.
    fn churn(s: &SqliteStore, e: &Entry, scope: &str, n: usize) {
        let tx = s.conn.unchecked_transaction().unwrap();
        let base = now_ms() + 1_000;
        for i in 0..n {
            let mut v = e.clone();
            v.scope = scope.into();
            v.body = format!("draft {i}");
            let closed = if i + 1 < n { Some(base + i as i64 + 1) } else { None };
            tx.execute(
                "INSERT INTO entries(uri,kind,namespace,title,body,tags,importance,dedup_key,\
                 created_ms,valid_from_ms,valid_to_ms,system_from_ms,system_to_ms,scope) \
                 VALUES (?1,?2,?3,?4,?5,'[]',?6,?7,?8,?9,NULL,?10,?11,?12)",
                params![
                    v.uri, v.kind.as_str(), v.namespace, v.title, v.body, v.importance, v.dedup_key,
                    v.created_ms, v.valid_from_ms, base + i as i64, closed, v.scope
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn history_limit_is_exact_under_heavy_invisible_churn() {
        let mut s = mem_store();
        let g = mk(Kind::Runbook, "resources/workspace", "Churned runbook", "the global curated version");
        s.put(&g).unwrap();
        churn(&s, &g, "user:bob", 810);
        s.set_read_scopes(Some(vec!["room:x".into()]));
        let h = s.history(&g.uri, 200).unwrap();
        assert_eq!(h.len(), 1, "exactly the one visible version: {}", h.len());
        assert_eq!((h[0].scope.as_str(), h[0].body.as_str()), ("", "the global curated version"));
        assert_eq!(s.history(&g.uri, 1).unwrap().len(), 1, "even limit=1 reaches past the churn");
        // bob sees his own lineage, newest first, limit exact
        s.set_read_scopes(Some(vec!["user:bob".into()]));
        let h = s.history(&g.uri, 5).unwrap();
        assert_eq!(h.len(), 5);
        assert!(h.iter().all(|e| e.scope == "user:bob") && h[0].body == "draft 809");
        // full tenant: everything
        s.set_read_scopes(None);
        assert_eq!(s.history(&g.uri, 1_000).unwrap().len(), 811);
    }

    #[test]
    fn list_reads_limit_is_exact_under_invisible_churn() {
        let mut s = mem_store();
        // one visible, LOW-importance global record of each shape...
        let mut low = mk(Kind::Reminder, "agent/reminders", "Visible reminder", "zqx keyword visible");
        low.importance = 1;
        s.put(&low).unwrap();
        let mut gp = mk(Kind::Persona, "shared/governance", "Visible house rules", "shared rules");
        gp.importance = 1;
        s.put(&gp).unwrap();
        // ...under 60 invisible, high-importance, keyword-stronger records of the same shapes
        for i in 0..60 {
            let mut r = mk(Kind::Reminder, "agent/reminders", &format!("Bob reminder {i}"), "zqx zqx zqx keyword");
            r.scope = "user:bob".into();
            r.importance = 99;
            s.put(&r).unwrap();
            let mut p = mk(Kind::Persona, "shared/governance", &format!("Bob persona {i}"), "bob rules");
            p.scope = "user:bob".into();
            p.importance = 99;
            s.put(&p).unwrap();
        }
        s.set_read_scopes(Some(vec!["room:x".into()]));
        let recent = s.recent(5).unwrap();
        assert!(recent.iter().any(|e| e.uri == low.uri), "recent reaches the visible row: {recent:?}");
        assert!(recent.iter().all(|e| e.scope.is_empty()));
        assert_eq!(s.by_kind("reminder", 5).unwrap().iter().map(|e| &e.uri).collect::<Vec<_>>(), vec![&low.uri]);
        let boot = s.by_kind_for_agent("persona", Some("assistant"), 5).unwrap();
        assert_eq!(boot.iter().map(|e| &e.uri).collect::<Vec<_>>(), vec![&gp.uri], "persona boot layer too");
        let hits = s.recall("zqx", 3).unwrap();
        assert_eq!(hits.iter().map(|e| &e.uri).collect::<Vec<_>>(), vec![&low.uri], "keyword channel too");
        let asof = s.recall_as_of("zqx", 3, now_ms(), now_ms()).unwrap();
        assert_eq!(asof.iter().map(|e| &e.uri).collect::<Vec<_>>(), vec![&low.uri], "as-of too");
    }

    #[test]
    fn sql_agent_tree_gate_matches_the_row_gate() {
        let mut s = mem_store();
        let own = mk(Kind::Persona, "agents/assistant/persona", "Own persona", "mine");
        let foreign = mk(Kind::Persona, "agents/analyst/persona", "Foreign persona", "theirs");
        let foreign_case = mk(Kind::Persona, "Agents/Analyst", "Foreign case persona", "theirs, odd case");
        let own_case = mk(Kind::Persona, "AGENTS/Assistant/x", "Own case persona", "mine, odd case");
        let lookalike = mk(Kind::Persona, "agents/assistantx", "Lookalike persona", "not mine");
        let shared = mk(Kind::Persona, "shared/governance", "Shared rules", "everyone");
        let bare = mk(Kind::Persona, "agents", "Bare agents namespace", "outside the tree");
        let all = [&own, &foreign, &foreign_case, &own_case, &lookalike, &shared, &bare];
        for e in all {
            s.put(e).unwrap();
        }
        s.set_agent_view(Some("assistant".into()));
        let expect: Vec<&str> = all.iter().filter(|e| s.row_ok(e)).map(|e| e.title.as_str()).collect();
        assert_eq!(expect.len(), 4, "own, own (case), shared, bare: {expect:?}");
        let mut got: Vec<String> = s.recent(50).unwrap().into_iter().map(|e| e.title).collect();
        got.sort();
        let mut want: Vec<String> = expect.iter().map(|t| t.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "the SQL gate admits exactly what row_ok admits");
        let mut boot: Vec<String> =
            s.by_kind_for_agent("persona", Some("assistant"), 50).unwrap().into_iter().map(|e| e.title).collect();
        boot.sort();
        assert_eq!(boot, want, "the persona route applies the same rule");
        assert!(s.history(&foreign_case.uri, 10).unwrap().is_empty(), "history of a foreign tree, any case");
        assert_eq!(s.history(&own_case.uri, 10).unwrap().len(), 1);
    }

    // --- E4: the newest VISIBLE row wins ---

    #[test]
    fn get_and_scope_visible_pick_the_newest_visible_row() {
        let mut s = mem_store();
        let g = mk_in("", "operator curated");
        s.put(&g).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.put(&mk_in("room:a", "room shadow")).unwrap(); // newer, same uri, another audience
        s.set_read_scopes(Some(vec!["room:b".into()]));
        let e = s.get(&g.uri).unwrap().expect("the global record is not hidden by room a's newer row");
        assert_eq!((e.scope.as_str(), e.body.as_str()), ("", "operator curated"));
        assert!(s.scope_visible(&g.uri).unwrap(), "the uri is on room b's graph");
        s.set_read_scopes(Some(vec!["room:a".into()]));
        assert_eq!(s.get(&g.uri).unwrap().unwrap().scope, "room:a", "room a's newest visible row is its own");
        s.set_read_scopes(None);
        assert_eq!(s.get(&g.uri).unwrap().unwrap().scope, "room:a", "full tenant: the newest row, as before");
        // only room a holds it: invisible to room b (live, just not theirs)
        s.forget_in(&g.uri, Some("")).unwrap();
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert!(s.get(&g.uri).unwrap().is_none());
        assert!(!s.scope_visible(&g.uri).unwrap(), "an invisible live row keeps the uri hidden");
    }

    #[test]
    fn get_in_scope_hydrates_exactly_that_record() {
        let mut s = mem_store();
        let g = mk_in("", "operator curated");
        s.put(&g).unwrap();
        s.put(&mk_in("room:a", "room shadow")).unwrap();
        s.set_read_scopes(Some(vec!["room:b".into()]));
        assert_eq!(s.get_in_scope(&g.uri, "").unwrap().unwrap().body, "operator curated");
        assert!(s.get_in_scope(&g.uri, "room:a").unwrap().is_none(), "room a's record stays invisible to room b");
        s.set_read_scopes(Some(vec!["room:a".into()]));
        assert_eq!(s.get_in_scope(&g.uri, "room:a").unwrap().unwrap().body, "room shadow");
        assert_eq!(s.get_in_scope(&g.uri, "").unwrap().unwrap().body, "operator curated");
        assert!(s.get_in_scope(&g.uri, "room:c").unwrap().is_none());
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

    /// Two connections on one tenant file, as a second process or a replication sidecar has.
    fn two_stores() -> (SqliteStore, SqliteStore) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmtest-tx-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        (SqliteStore::open(&path).unwrap(), SqliteStore::open(&path).unwrap())
    }

    // P0-1: a transaction takes the write lock at BEGIN (IMMEDIATE), not at its first write. While
    // one connection's transaction is open, another connection that will not wait cannot write.
    #[test]
    fn transactions_take_the_write_lock_at_begin() {
        let (a, b) = two_stores();
        b.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let tx = a.conn.unchecked_transaction().unwrap();
        let busy = b.bump_signal("u", 1).expect_err("the open transaction already holds the write lock");
        assert!(format!("{busy:#}").contains("locked"), "{busy:#}");
        drop(tx); // rolls back and releases the lock
        b.bump_signal("u", 1).unwrap();
    }

    // A recall with no hits bumps nothing and so takes no write lock: while another connection
    // holds the write lock, it returns at once instead of waiting up to busy_timeout.
    #[test]
    fn bumping_no_signals_takes_no_write_lock() {
        let (a, b) = two_stores();
        b.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let tx = a.conn.unchecked_transaction().unwrap();
        a.bump_signal("held", 1).unwrap(); // the transaction now holds the write lock
        b.bump_signals(&[], 1).expect("an empty bump needs no lock");
        assert!(b.bump_signals(&["u"], 1).is_err(), "a real bump still needs the lock");
        drop(tx);
    }

    // P0-1: several connections writing one file at once (every put reads, then writes, in one
    // transaction) all succeed: each waits its turn for the write lock (busy_timeout) instead of
    // failing its upgrade with "database is locked" when another commit landed first.
    #[test]
    fn concurrent_writers_on_one_file_never_fail_as_locked() {
        let (a, _b) = two_stores();
        let path: std::path::PathBuf = a.conn.path().map(std::path::PathBuf::from).unwrap();
        drop(a);
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let s = SqliteStore::open(&path).unwrap();
                    let mut errors = Vec::new();
                    for i in 0..40 {
                        let e = mk(Kind::Memory, "ns", &format!("w{t}-{i}"), &format!("body {t} {i}"));
                        if let Err(err) = s.put(&e) {
                            errors.push(format!("{err:#}"));
                        }
                        let key = format!("w{t}-{i}");
                        if let Err(err) = s.bump_signals(&[key.as_str(), "shared"], now_ms()) {
                            errors.push(format!("{err:#}"));
                        }
                    }
                    errors
                })
            })
            .collect();
        let errors: Vec<String> = threads.into_iter().flat_map(|h| h.join().unwrap()).collect();
        assert!(errors.is_empty(), "{} write(s) failed, e.g. {:?}", errors.len(), errors.first());
        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(s.recent(1000).unwrap().len(), 160, "every put landed");
    }
}
