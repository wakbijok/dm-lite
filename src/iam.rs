//! Token-only IAM for server mode (decision ac6f3688). `<data>/iam.db` holds tenants and
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
}

pub struct Iam {
    conn: Connection,
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
        let conn = Connection::open(path)?;
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
        Ok(Self { conn })
    }

    /// Ensure a root admin token exists. Returns Some(plaintext) only when a NEW bootstrap
    /// token was generated (so the caller can display/persist it once). `$DM_ADMIN_TOKEN`
    /// registers a fixed admin token instead.
    pub fn ensure_admin(&self) -> Result<Option<String>> {
        if let Ok(t) = std::env::var("DM_ADMIN_TOKEN") {
            if !t.is_empty() {
                self.conn.execute(
                    "INSERT OR IGNORE INTO tokens(token_hash,tenant,is_admin,label,created_ms) \
                     VALUES(?1,NULL,1,'env',?2)",
                    params![sha256_hex(&t), crate::entry::now_ms()],
                )?;
                return Ok(None);
            }
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
        if let Ok(dir) = crate::config::data_dir() {
            let p = dir.join("admin.token");
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
        let (tenant, is_admin, agent): (Option<String>, i64, Option<String>) = self
            .conn
            .query_row(
                "SELECT tenant, is_admin, agent FROM tokens WHERE token_hash=?1 AND revoked=0",
                params![h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()?;
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
        Some(Identity { tenant, is_admin: is_admin != 0, agent })
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

    /// (tenant, status, live-token-count, distinct agent labels) per tenant. `agents` is the
    /// comma-joined distinct agent labels of the tenant's live tokens (empty = agent-less only).
    pub fn list(&self) -> Result<Vec<(String, String, i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tenant, t.status, \
             (SELECT COUNT(*) FROM tokens k WHERE k.tenant=t.tenant AND k.revoked=0), \
             (SELECT COALESCE(GROUP_CONCAT(DISTINCT k2.agent), '') FROM tokens k2 \
              WHERE k2.tenant=t.tenant AND k2.revoked=0 AND k2.agent IS NOT NULL) \
             FROM tenants t ORDER BY t.tenant",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
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
        let t1 = iam.ensure_admin().unwrap().expect("new admin token");
        assert!(iam.ensure_admin().unwrap().is_none(), "second call does not re-issue");
        let id = iam.resolve(&t1).expect("admin token resolves");
        assert!(id.is_admin && id.tenant.is_none());
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
        let (_t, tok) = iam.create_tenant("wakspace", "", "izu-bridge", Some("Izu")).unwrap();
        let id = iam.resolve(&tok).expect("agent token resolves");
        assert_eq!(id.tenant.as_deref(), Some("wakspace"));
        assert_eq!(id.agent.as_deref(), Some("izu"), "agent label is canonicalized (lowercase)");
        // a second token on the SAME tenant with a different agent: shared memory, distinct identity
        let (_t2, tok2) = iam.create_tenant("wakspace", "", "shesta-bridge", Some("shesta")).unwrap();
        assert_eq!(iam.resolve(&tok2).unwrap().agent.as_deref(), Some("shesta"));
        assert_eq!(iam.resolve(&tok).unwrap().agent.as_deref(), Some("izu"), "first token unchanged");
        // list surfaces the agent labels for the tenant
        let rows = iam.list().unwrap();
        let row = rows.iter().find(|r| r.0 == "wakspace").expect("tenant listed");
        assert!(row.3.contains("izu") && row.3.contains("shesta"), "agents listed: {}", row.3);
        // admin tokens stay agent-less
        let admin = iam.ensure_admin().unwrap().expect("new admin token");
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
        drop(iam);
        let iam2 = Iam::open_at(&path).expect("second open is a no-op (idempotent)");
        assert!(iam2.resolve(legacy_secret).is_some());
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
}
