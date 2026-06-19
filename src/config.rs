//! Paths + tenant resolution. Embedded mode = one tenant ("default"); server mode
//! (later) selects the tenant per request, one database file per tenant.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Base data dir: $DM_DATA_DIR, else ~/.local/share/dm (XDG-ish), else ~/.dm.
pub fn data_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("DM_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = dirs::data_dir() {
        return Ok(d.join("dm"));
    }
    if let Some(h) = dirs::home_dir() {
        return Ok(h.join(".dm"));
    }
    Err(anyhow!("could not resolve a data directory"))
}

/// Canonical tenant identity: lowercased and restricted to [a-z0-9_-]; empty -> "default".
/// Used by BOTH auth and path derivation so one logical tenant maps to exactly one store
/// regardless of case or punctuation (tenant names are case-insensitive), and so the record
/// store and its vector index never diverge.
pub fn canonical_tenant(raw: &str) -> String {
    let safe: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if safe.is_empty() {
        "default".to_string()
    } else {
        safe
    }
}

/// Database file for a tenant. Database-per-tenant: physical isolation per tenant.
pub fn db_path(tenant: &str) -> Result<PathBuf> {
    let dir = data_dir()?.join("tenants");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{}.db", canonical_tenant(tenant))))
}

/// Per-tenant vector-index directory (zvec). Same canonical tenant as `db_path`, so the
/// SQLite record store and its vector index for a tenant always co-locate consistently.
#[cfg_attr(not(feature = "zvec"), allow(dead_code))]
pub fn vector_dir(tenant: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("vectors").join(canonical_tenant(tenant)))
}

/// Resolve the active tenant: $DM_TENANT, else "default".
pub fn tenant() -> String {
    std::env::var("DM_TENANT").unwrap_or_else(|_| "default".to_string())
}
