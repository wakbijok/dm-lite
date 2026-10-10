//! Token-only IAM for server mode. `<data>/iam.db` holds tenants and
//! hashed tokens; a bootstrap ROOT admin token (no tenant, no memory) authorizes admin ops.
//! A member token maps to exactly one tenant -> database-per-tenant isolation. No passwords.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// What a token resolves to. `tenant == None && is_admin` is the root admin. `agent` is the
/// optional per-agent identity label a token carries (persona selection + write attribution);
/// tokens minted before the agent column exist read back as agent-less (None).
#[derive(Debug, Clone)]
pub struct Identity {
    pub tenant: Option<String>,
    pub is_admin: bool,
    pub agent: Option<String>,
    /// Scope primitive. `scope_read`: None = full tenant
    /// (every scope; all pre-scope tokens); Some(set) = those scopes + global. `scope_write`:
    /// the single scope stamped on this token's writes; None on a scope-bound token means
    /// the token cannot write at all (decision Q3: global writes from scoped tokens are
    /// forbidden - promotion is explicit). `adapter`: trusted verdict-propagation component
    /// (decision Q1) - may assert the reader's scope set per request via headers.
    pub scope_read: Option<Vec<String>>,
    pub scope_write: Option<String>,
    pub adapter: bool,
}

impl Identity {
    /// A full-tenant identity (no scope restriction) - what every pre-scope token resolves to.
    pub fn scope_unbound(&self) -> bool {
        self.scope_read.is_none() && self.scope_write.is_none() && !self.adapter
    }
}

pub struct Iam {
    conn: Connection,
    /// The database file (`<data>/iam.db` in production). A bootstrap admin token is written
    /// beside it, and readiness checks it is still there.
    path: PathBuf,
}

/// A tenant as `Iam::list` reports it.
#[derive(Debug, Clone)]
pub struct TenantRow {
    pub tenant: String,
    pub status: String,
    /// Live (unrevoked) tokens.
    pub tokens: i64,
    /// Comma-joined distinct agent labels of the live tokens (empty = agent-less only).
    pub agents: String,
    /// Tenant policy: writes by full-tenant tokens must name their scope (`X-DM-Write-Scope`).
    pub require_explicit_write_scope: bool,
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    to_hex(&Sha256::digest(s.as_bytes()))
}

/// A fresh opaque token: `dmem_` + 160 random bits, hex.
fn gen_token() -> Result<String> {
    let mut buf = [0u8; 20];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("rng: {e}"))?;
    Ok(format!("dmem_{}", to_hex(&buf)))
}

fn iam_db_path() -> Result<PathBuf> {
    Ok(crate::config::data_dir()?.join("iam.db"))
}

impl Iam {
    pub fn open() -> Result<Self> {
        Self::open_at(&iam_db_path()?)
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).ok();
        }
        let mut conn = Connection::open(path)?;
        // Any transaction takes the write lock at BEGIN, as on the tenant stores (see
        // `SqliteStore::open`). Every write here is a single autocommit statement today; this
        // keeps a future multi-statement one from failing on a stale-snapshot upgrade.
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        // iam.db holds token hashes and the tenant registry; owner-only (audit Medium #5).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS tenants(
                tenant TEXT PRIMARY KEY,
                display TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'active',
                created_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tokens(
                token_hash TEXT PRIMARY KEY,
                tenant TEXT,
                is_admin INTEGER NOT NULL DEFAULT 0,
                label TEXT NOT NULL DEFAULT '',
                created_ms INTEGER NOT NULL,
                last_used_ms INTEGER,
                revoked INTEGER NOT NULL DEFAULT 0,
                agent TEXT);",
        )?;
        // Idempotent migration for a pre-agent iam.db: the fresh CREATE TABLE above already
        // carries `agent`; an existing tokens table gets the column added here. Nullable, so
        // every pre-existing token reads back as agent-less (exactly its old behaviour).
        let has_agent: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tokens') WHERE name='agent'",
            [],
            |r| r.get(0),
        )?;
        if has_agent == 0 {
            conn.execute_batch("ALTER TABLE tokens ADD COLUMN agent TEXT")?;
        }
        // Scope-primitive columns, same idempotent pattern. All nullable/zero-default: every
        // pre-scope token reads back scope-unbound (full tenant - exactly its old behavior).
        let has_scope: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tokens') WHERE name='scope_read'",
            [],
            |r| r.get(0),
        )?;
        if has_scope == 0 {
            conn.execute_batch(
                "ALTER TABLE tokens ADD COLUMN scope_read TEXT;
                 ALTER TABLE tokens ADD COLUMN scope_write TEXT;
                 ALTER TABLE tokens ADD COLUMN adapter INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        // Tenant write-scope policy (0.3.7), same idempotent pattern. Zero default: every
        // existing tenant keeps its old behaviour until an admin turns the policy on. An older
        // dmem opening this db ignores the column (its queries name their columns, and its
        // tenant inserts take the default).
        let has_policy: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tenants') WHERE name='require_explicit_write_scope'",
            [],
            |r| r.get(0),
        )?;
        if has_policy == 0 {
            conn.execute_batch(
                "ALTER TABLE tenants ADD COLUMN require_explicit_write_scope INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        Ok(Self { conn, path: path.to_path_buf() })
    }

    /// Is the IAM database still there and readable? Readiness asks this: an open connection
    /// keeps reading a file that was deleted or replaced under it, so the file must also still
    /// be at its path.
    pub fn healthy(&self) -> Result<()> {
        if !self.path.exists() {
            return Err(anyhow!("the IAM database file is missing from its path"));
        }
        self.conn.query_row("SELECT COUNT(*) FROM tenants", [], |r| r.get::<_, i64>(0))?;
        Ok(())
    }

    /// Where a bootstrap admin token is written (`admin.token` beside `iam.db`).
    pub fn admin_token_path(&self) -> Option<PathBuf> {
        self.path.parent().map(|d| d.join("admin.token"))
    }

    /// Does `tenant` require every write by a full-tenant (scope-unbound) token to name its
    /// scope in `X-DM-Write-Scope`? A tenant without a row here (an env-token-only tenant)
    /// has no policy: false.
    pub fn requires_explicit_write_scope(&self, tenant: &str) -> Result<bool> {
        let t = crate::config::canonical_tenant(tenant);
        let on: Option<i64> = self
            .conn
            .query_row(
                "SELECT require_explicit_write_scope FROM tenants WHERE tenant=?1",
                params![t],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        Ok(on.unwrap_or(0) != 0)
    }

    /// Turn a tenant's explicit-write-scope policy on or off. The tenant must already exist
    /// here (a typo must not quietly register a new tenant); the change applies from the next
    /// request, since the server reads the policy per request.
    pub fn set_requires_explicit_write_scope(&self, tenant: &str, on: bool) -> Result<()> {
        let t = crate::config::canonical_tenant(tenant);
        let n = self.conn.execute(
            "UPDATE tenants SET require_explicit_write_scope=?2 WHERE tenant=?1",
            params![t, on as i64],
        )?;
        if n == 0 {
            return Err(crate::tools::Refusal::new(format!("unknown tenant '{t}' (create it with `dmem admin add` first)")).into());
        }
        Ok(())
    }

    /// Ensure a root admin token exists. Returns Some(plaintext) only when a NEW bootstrap
    /// token was generated (so the caller can display/persist it once). `$DM_ADMIN_TOKEN`
    /// registers a fixed admin token instead.
    ///
    /// Whenever `DM_ADMIN_TOKEN` is present in the environment, no bootstrap token is ever
    /// generated, printed or written, even when its value is empty or not UTF-8 (a Secret
    /// mounted with a missing key, say): the operator chose to supply the admin token, and a
    /// freshly minted one would land in the logs. Such a value registers nothing and says so.
    ///
    /// A non-empty value is THE env admin token: admin tokens registered from an earlier
    /// `DM_ADMIN_TOKEN` value (rotated, say after a leak) are revoked, so the old value stops
    /// working at the next start. Bootstrap and other admin tokens are left alone, and an empty
    /// or unreadable value revokes nothing (it may be a mis-mounted Secret, not a rotation).
    pub fn ensure_admin(&self) -> Result<Option<String>> {
        self.ensure_admin_from(std::env::var_os("DM_ADMIN_TOKEN"))
    }

    /// `ensure_admin` with the `DM_ADMIN_TOKEN` value passed in (None = not set).
    pub(crate) fn ensure_admin_from(&self, env_token: Option<std::ffi::OsString>) -> Result<Option<String>> {
        if let Some(raw) = env_token {
            match raw.to_str() {
                Some(t) if !t.is_empty() => {
                    let hash = sha256_hex(t);
                    let tx = self.conn.unchecked_transaction()?;
                    tx.execute(
                        "INSERT OR IGNORE INTO tokens(token_hash,tenant,is_admin,label,created_ms) \
                         VALUES(?1,NULL,1,'env',?2)",
                        params![hash, crate::entry::now_ms()],
                    )?;
                    let stale = tx.execute(
                        "UPDATE tokens SET revoked=1 WHERE is_admin=1 AND label='env' AND revoked=0 AND token_hash<>?1",
                        params![hash],
                    )?;
                    tx.commit()?;
                    if stale > 0 {
                        eprintln!(
                            "dmem: revoked {stale} admin token(s) registered from an earlier DM_ADMIN_TOKEN value; \
                             only the current value is the env admin token"
                        );
                    }
                }
                _ => eprintln!(
                    "dmem: DM_ADMIN_TOKEN is set but empty (or not UTF-8): no admin token registered from it, \
                     and no bootstrap token generated. Admin routes accept only admin tokens already in iam.db."
                ),
            }
            return Ok(None);
        }
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tokens WHERE is_admin=1 AND revoked=0", [], |r| r.get(0))?;
        if n > 0 {
            return Ok(None);
        }
        let token = gen_token()?;
        self.conn.execute(
            "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms) VALUES(?1,NULL,1,'bootstrap',?2)",
            params![sha256_hex(&token), crate::entry::now_ms()],
        )?;
        if let Some(p) = self.admin_token_path() {
            // 0600 from creation (atomic temp+rename); surface a failure instead of swallowing it
            // so a world-readable admin token never goes unnoticed.
            if let Err(e) = crate::config::write_secret(&p, &token) {
                eprintln!("dmem: could not write {} securely ({e}); the admin token is shown above", p.display());
            }
        }
        Ok(Some(token))
    }

    /// Resolve a presented token to an identity (None = unknown/revoked/suspended). Read-only: it
    /// does NOT write last_used_ms. The server resolves on EVERY request, and a per-request UPDATE
    /// serializes all auth on the IAM write lock; the timestamp is not surfaced anywhere, so it is
    /// not worth that contention. (The column stays for schema compatibility.)
    pub fn resolve(&self, token: &str) -> Option<Identity> {
        let h = sha256_hex(token);
        let (tenant, is_admin, agent, scope_read_json, scope_write, adapter): (
            Option<String>,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ) = self
            .conn
            .query_row(
                "SELECT tenant, is_admin, agent, scope_read, scope_write, adapter \
                 FROM tokens WHERE token_hash=?1 AND revoked=0",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .ok()?;
        // Malformed stored JSON degrades to an EMPTY grant set (global only), never to full
        // tenant - a corrupt row must fail closed.
        let scope_read: Option<Vec<String>> =
            scope_read_json.map(|j| serde_json::from_str(&j).unwrap_or_default());
        if let Some(t) = &tenant {
            let active: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM tenants WHERE tenant=?1 AND status='active'",
                    params![t],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if active == 0 {
                return None;
            }
        }
        Some(Identity { tenant, is_admin: is_admin != 0, agent, scope_read, scope_write, adapter: adapter != 0 })
    }

    /// Mint a scope-bound (or adapter) member token for an existing-or-new tenant.
    /// `scope_read` = the scopes this token may read (global always rides along);
    /// `scope_write` = the single scope stamped on its writes (None = cannot write, Q3);
    /// `adapter` = trusted to assert the reader's scopes per request (Q1). The plaintext
    /// token is returned exactly once.
    pub fn mint_scoped_token(
        &self,
        tenant: &str,
        label: &str,
        agent: Option<&str>,
        scope_read: &[String],
        scope_write: Option<&str>,
        adapter: bool,
    ) -> Result<String> {
        let t = crate::config::canonical_tenant(tenant);
        let agent = agent.and_then(crate::config::canonical_agent);
        self.conn.execute(
            "INSERT OR IGNORE INTO tenants(tenant,display,status,created_ms) VALUES(?1,'','active',?2)",
            params![t, crate::entry::now_ms()],
        )?;
        let token = gen_token()?;
        // For an ADAPTER, an empty scope_read means "no cap" (assertions are per-request, Q1)
        // and is stored NULL; a non-adapter's empty list is a real grant set (global-only)
        // and is stored as [] - the two must not be conflated, or the cap filter would
        // silently reduce every adapter assertion to nothing.
        let scope_read_json: Option<String> = if adapter && scope_read.is_empty() {
            None
        } else {
            Some(serde_json::to_string(scope_read)?)
        };
        self.conn.execute(
            "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms,agent,scope_read,scope_write,adapter) \
             VALUES(?1,?2,0,?3,?4,?5,?6,?7,?8)",
            params![
                sha256_hex(&token),
                t,
                label,
                crate::entry::now_ms(),
                agent,
                scope_read_json,
                scope_write,
                adapter as i64
            ],
        )?;
        Ok(token)
    }

    /// Create the tenant if absent and issue a new member token (returned in plaintext once).
    /// `agent` optionally labels the token with a per-agent identity (canonicalized); None
    /// mints an agent-less token (today's behaviour).
    pub fn create_tenant(&self, tenant: &str, display: &str, label: &str, agent: Option<&str>) -> Result<(String, String)> {
        let t = crate::config::canonical_tenant(tenant);
        let agent = agent.and_then(crate::config::canonical_agent);
        self.conn.execute(
            "INSERT OR IGNORE INTO tenants(tenant,display,status,created_ms) VALUES(?1,?2,'active',?3)",
            params![t, display, crate::entry::now_ms()],
        )?;
        // re-activate if previously suspended
        self.conn.execute("UPDATE tenants SET status='active' WHERE tenant=?1", params![t])?;
        let token = gen_token()?;
        self.conn.execute(
            "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms,agent) VALUES(?1,?2,0,?3,?4,?5)",
            params![sha256_hex(&token), t, label, crate::entry::now_ms(), agent],
        )?;
        Ok((t, token))
    }

    /// One row per tenant: status, live-token count, the comma-joined distinct agent labels of
    /// its live tokens (empty = agent-less only), and its explicit-write-scope policy.
    pub fn list(&self) -> Result<Vec<TenantRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tenant, t.status, \
             (SELECT COUNT(*) FROM tokens k WHERE k.tenant=t.tenant AND k.revoked=0), \
             (SELECT COALESCE(GROUP_CONCAT(DISTINCT k2.agent), '') FROM tokens k2 \
              WHERE k2.tenant=t.tenant AND k2.revoked=0 AND k2.agent IS NOT NULL), \
             t.require_explicit_write_scope \
             FROM tenants t ORDER BY t.tenant",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TenantRow {
                    tenant: r.get(0)?,
                    status: r.get(1)?,
                    tokens: r.get(2)?,
                    agents: r.get(3)?,
                    require_explicit_write_scope: r.get::<_, i64>(4)? != 0,
                })
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    }

    /// Revoke a single token (by its plaintext), or all live tokens of a tenant. Count revoked.
    pub fn revoke(&self, token_or_tenant: &str) -> Result<usize> {
        let n = self
            .conn
            .execute("UPDATE tokens SET revoked=1 WHERE token_hash=?1 AND revoked=0", params![sha256_hex(token_or_tenant)])?;
        if n > 0 {
            return Ok(n);
        }
        let t = crate::config::canonical_tenant(token_or_tenant);
        Ok(self.conn.execute("UPDATE tokens SET revoked=1 WHERE tenant=?1 AND revoked=0", params![t])?)
    }

    /// Suspend a tenant and revoke its tokens (memory data is left on disk).
    pub fn remove_tenant(&self, tenant: &str) -> Result<()> {
        let t = crate::config::canonical_tenant(tenant);
        self.conn.execute("UPDATE tenants SET status='suspended' WHERE tenant=?1", params![t])?;
        self.conn.execute("UPDATE tokens SET revoked=1 WHERE tenant=?1", params![t])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> Iam {
        let n = C.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmiam-{}-{}-{}", std::process::id(), crate::entry::now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        Iam::open_at(&dir.join("iam.db")).unwrap()
    }

    #[test]
    fn admin_bootstrap_is_idempotent_and_resolves() {
        let iam = tmp();
        let t1 = iam.ensure_admin_from(None).unwrap().expect("new admin token");
        assert!(iam.ensure_admin_from(None).unwrap().is_none(), "second call does not re-issue");
        let id = iam.resolve(&t1).expect("admin token resolves");
        assert!(id.is_admin && id.tenant.is_none());
        // written beside this iam.db (never into the process's data dir), owner-only
        let p = iam.admin_token_path().unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), t1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    // P0-6: with DM_ADMIN_TOKEN in the environment no bootstrap token is ever minted, printed or
    // written, whatever its value; a non-empty value is the admin token.
    #[test]
    fn env_admin_token_never_mints_or_writes_a_bootstrap_token() {
        let iam = tmp();
        let env = "env-admin-0123456789abcdef";
        assert!(iam.ensure_admin_from(Some(env.into())).unwrap().is_none());
        assert!(iam.resolve(env).is_some_and(|id| id.is_admin && id.tenant.is_none()));
        assert!(!iam.admin_token_path().unwrap().exists(), "no admin.token written");
        // set but empty (a Secret key that is missing): still no bootstrap, and nothing registered
        let iam = tmp();
        assert!(iam.ensure_admin_from(Some("".into())).unwrap().is_none());
        assert!(!iam.admin_token_path().unwrap().exists());
        let admins: i64 = iam.conn.query_row("SELECT COUNT(*) FROM tokens WHERE is_admin=1", [], |r| r.get(0)).unwrap();
        assert_eq!(admins, 0, "an empty DM_ADMIN_TOKEN registers nothing and mints nothing");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let iam = tmp();
            assert!(iam.ensure_admin_from(Some(std::ffi::OsString::from_vec(vec![0xff, 0xfe]))).unwrap().is_none());
            assert!(!iam.admin_token_path().unwrap().exists());
        }
    }

    // Rotating DM_ADMIN_TOKEN (a leaked Secret, say) retires the old value at the next start:
    // only the current env value stays an admin token. Other admin tokens (the bootstrap one)
    // are untouched, and an empty value (a mis-mounted Secret) revokes nothing.
    #[test]
    fn rotating_the_env_admin_token_revokes_the_old_value() {
        let iam = tmp();
        let boot = iam.ensure_admin_from(None).unwrap().expect("bootstrap admin");
        let (old, new) = ("env-admin-old-0123456789abcdef", "env-admin-new-0123456789abcdef");
        iam.ensure_admin_from(Some(old.into())).unwrap();
        assert!(iam.resolve(old).is_some_and(|id| id.is_admin));
        iam.ensure_admin_from(Some("".into())).unwrap();
        assert!(iam.resolve(old).is_some_and(|id| id.is_admin), "an empty value revokes nothing");
        iam.ensure_admin_from(Some(new.into())).unwrap();
        assert!(iam.resolve(new).is_some_and(|id| id.is_admin), "the new value is the admin token");
        assert!(iam.resolve(old).is_none(), "the rotated-out value no longer resolves");
        assert!(iam.resolve(&boot).is_some_and(|id| id.is_admin), "the bootstrap token is not an env token");
        // idempotent: the same value again changes nothing
        iam.ensure_admin_from(Some(new.into())).unwrap();
        assert!(iam.resolve(new).is_some_and(|id| id.is_admin));
    }

    #[test]
    fn member_token_maps_to_its_tenant_only() {
        let iam = tmp();
        let (tenant, tok) = iam.create_tenant("ACME", "Acme Inc", "laptop", None).unwrap();
        assert_eq!(tenant, "acme");
        let id = iam.resolve(&tok).expect("member token resolves");
        assert_eq!(id.tenant.as_deref(), Some("acme"));
        assert!(!id.is_admin);
        assert!(id.agent.is_none(), "agent-less token carries no agent");
        assert!(iam.resolve("dmem_bogus").is_none());
    }

    #[test]
    fn agent_token_round_trips_canonicalized() {
        let iam = tmp();
        let (_t, tok) = iam.create_tenant("workspace", "", "assistant-bridge", Some("Assistant")).unwrap();
        let id = iam.resolve(&tok).expect("agent token resolves");
        assert_eq!(id.tenant.as_deref(), Some("workspace"));
        assert_eq!(id.agent.as_deref(), Some("assistant"), "agent label is canonicalized (lowercase)");
        // a second token on the SAME tenant with a different agent: shared memory, distinct identity
        let (_t2, tok2) = iam.create_tenant("workspace", "", "analyst-bridge", Some("analyst")).unwrap();
        assert_eq!(iam.resolve(&tok2).unwrap().agent.as_deref(), Some("analyst"));
        assert_eq!(iam.resolve(&tok).unwrap().agent.as_deref(), Some("assistant"), "first token unchanged");
        // list surfaces the agent labels for the tenant
        let rows = iam.list().unwrap();
        let row = rows.iter().find(|r| r.tenant == "workspace").expect("tenant listed");
        assert!(row.agents.contains("assistant") && row.agents.contains("analyst"), "agents listed: {}", row.agents);
        // admin tokens stay agent-less
        let admin = iam.ensure_admin_from(None).unwrap().expect("new admin token");
        assert!(iam.resolve(&admin).unwrap().agent.is_none());
    }

    #[test]
    fn pre_agent_iam_db_migrates_idempotently() {
        // Simulate an iam.db created BEFORE the agent column existed, then open it normally:
        // the column is added, old tokens resolve agent-less, and a re-open is a no-op.
        let n = C.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmiam-old-{}-{}-{}", std::process::id(), crate::entry::now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("iam.db");
        let legacy_secret = "dmem_legacy_token";
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tenants(tenant TEXT PRIMARY KEY, display TEXT NOT NULL DEFAULT '', \
                     status TEXT NOT NULL DEFAULT 'active', created_ms INTEGER NOT NULL);
                 CREATE TABLE tokens(token_hash TEXT PRIMARY KEY, tenant TEXT, \
                     is_admin INTEGER NOT NULL DEFAULT 0, label TEXT NOT NULL DEFAULT '', \
                     created_ms INTEGER NOT NULL, last_used_ms INTEGER, \
                     revoked INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tenants(tenant,display,status,created_ms) VALUES('legacy','','active',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms) VALUES(?1,'legacy',0,'old',1)",
                params![sha256_hex(legacy_secret)],
            )
            .unwrap();
        }
        let iam = Iam::open_at(&path).expect("open migrates the old schema");
        let id = iam.resolve(legacy_secret).expect("pre-agent token still resolves");
        assert_eq!(id.tenant.as_deref(), Some("legacy"));
        assert!(id.agent.is_none(), "pre-agent token reads back agent-less");
        assert!(!iam.requires_explicit_write_scope("legacy").unwrap(), "pre-policy tenant: policy off");
        drop(iam);
        let iam2 = Iam::open_at(&path).expect("second open is a no-op (idempotent)");
        assert!(iam2.resolve(legacy_secret).is_some());
    }

    #[test]
    fn explicit_write_scope_policy_defaults_off_and_toggles_per_tenant() {
        let iam = tmp();
        let (_t, _tok) = iam.create_tenant("workspace", "", "", None).unwrap();
        iam.create_tenant("other", "", "", None).unwrap();
        assert!(!iam.requires_explicit_write_scope("workspace").unwrap(), "off by default");
        iam.set_requires_explicit_write_scope("WorkSpace", true).unwrap();
        assert!(iam.requires_explicit_write_scope("workspace").unwrap(), "canonical tenant name");
        assert!(!iam.requires_explicit_write_scope("other").unwrap(), "per tenant");
        assert!(!iam.requires_explicit_write_scope("env-only").unwrap(), "no row: no policy");
        let rows = iam.list().unwrap();
        let flag = |t: &str| rows.iter().find(|r| r.tenant == t).unwrap().require_explicit_write_scope;
        assert!(flag("workspace") && !flag("other"), "list reports the policy");
        // an unknown tenant is refused, never silently registered
        let e = iam.set_requires_explicit_write_scope("typo", true).expect_err("unknown tenant");
        assert!(e.downcast_ref::<crate::tools::Refusal>().is_some(), "{e:#}");
        assert!(iam.list().unwrap().iter().all(|r| r.tenant != "typo"));
        // re-issuing a token (create_tenant re-activates) keeps the policy
        iam.create_tenant("workspace", "", "second", None).unwrap();
        assert!(iam.requires_explicit_write_scope("workspace").unwrap());
        iam.set_requires_explicit_write_scope("workspace", false).unwrap();
        assert!(!iam.requires_explicit_write_scope("workspace").unwrap());
    }

    #[test]
    fn opens_a_0_3_6_iam_db_and_adds_the_policy_column() {
        // The 0.3.6 schema: agent + scope columns on tokens, no policy column on tenants.
        let n = C.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmiam-036-{}-{}-{}", std::process::id(), crate::entry::now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("iam.db");
        let (full_secret, scoped_secret) = ("dmem_full_036_token", "dmem_scoped_036_token");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tenants(tenant TEXT PRIMARY KEY, display TEXT NOT NULL DEFAULT '', \
                     status TEXT NOT NULL DEFAULT 'active', created_ms INTEGER NOT NULL);
                 CREATE TABLE tokens(token_hash TEXT PRIMARY KEY, tenant TEXT, \
                     is_admin INTEGER NOT NULL DEFAULT 0, label TEXT NOT NULL DEFAULT '', \
                     created_ms INTEGER NOT NULL, last_used_ms INTEGER, \
                     revoked INTEGER NOT NULL DEFAULT 0, agent TEXT, scope_read TEXT, \
                     scope_write TEXT, adapter INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO tenants(tenant,display,status,created_ms) VALUES('workspace','','active',1);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms) VALUES(?1,'workspace',0,'op',1)",
                params![sha256_hex(full_secret)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tokens(token_hash,tenant,is_admin,label,created_ms,scope_read,scope_write,adapter) \
                 VALUES(?1,'workspace',0,'bridge',1,NULL,NULL,1)",
                params![sha256_hex(scoped_secret)],
            )
            .unwrap();
        }
        let iam = Iam::open_at(&path).expect("open migrates the 0.3.6 schema");
        assert!(iam.resolve(full_secret).expect("full token resolves").scope_unbound());
        assert!(iam.resolve(scoped_secret).expect("adapter token resolves").adapter);
        assert!(!iam.requires_explicit_write_scope("workspace").unwrap(), "existing tenant: policy off");
        iam.set_requires_explicit_write_scope("workspace", true).unwrap();
        drop(iam);
        let iam = Iam::open_at(&path).expect("second open is a no-op");
        assert!(iam.requires_explicit_write_scope("workspace").unwrap(), "the policy persists");
        // an older dmem's tenant insert (no policy column named) still works on the new schema
        let conn = Connection::open(&path).unwrap();
        conn.execute("INSERT INTO tenants(tenant,display,status,created_ms) VALUES('late','','active',2)", []).unwrap();
        assert!(!iam.requires_explicit_write_scope("late").unwrap());
    }

    #[test]
    fn revoke_and_suspend_block_access() {
        let iam = tmp();
        let (_t, tok) = iam.create_tenant("globex", "", "", None).unwrap();
        assert!(iam.resolve(&tok).is_some());
        assert_eq!(iam.revoke(&tok).unwrap(), 1);
        assert!(iam.resolve(&tok).is_none(), "revoked token is dead");

        let (_t2, tok2) = iam.create_tenant("initech", "", "", None).unwrap();
        iam.remove_tenant("initech").unwrap();
        assert!(iam.resolve(&tok2).is_none(), "suspended tenant blocks its tokens");
    }

    #[test]
    fn scoped_and_adapter_tokens_roundtrip() {
        let iam = tmp();
        // pre-scope token: scope-unbound (full tenant), exactly its old behavior
        let (_t, plain) = iam.create_tenant("acme", "", "", None).unwrap();
        let id = iam.resolve(&plain).unwrap();
        assert!(id.scope_unbound(), "plain member token is full-tenant");

        let scoped = iam
            .mint_scoped_token("acme", "alice", None, &["user:alice".to_string()], Some("user:alice"), false)
            .unwrap();
        let id = iam.resolve(&scoped).unwrap();
        assert!(!id.scope_unbound());
        assert_eq!(id.scope_read.as_deref(), Some(&["user:alice".to_string()][..]));
        assert_eq!(id.scope_write.as_deref(), Some("user:alice"));
        assert!(!id.adapter);

        let readonly = iam.mint_scoped_token("acme", "ro", None, &["room:x".to_string()], None, false).unwrap();
        let id = iam.resolve(&readonly).unwrap();
        assert!(id.scope_write.is_none(), "no write scope = read-only token (Q3)");

        let adapter = iam.mint_scoped_token("acme", "bridge", None, &[], None, true).unwrap();
        let id = iam.resolve(&adapter).unwrap();
        assert!(id.adapter, "adapter flag survives the roundtrip");
    }
}
