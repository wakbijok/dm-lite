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

/// Database file for a tenant. Database-per-tenant: physical isolation per tenant.
pub fn db_path(tenant: &str) -> Result<PathBuf> {
    let safe: String = tenant
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let dir = data_dir()?.join("tenants");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{}.db", if safe.is_empty() { "default".into() } else { safe })))
}

/// Resolve the active tenant: $DM_TENANT, else "default".
pub fn tenant() -> String {
    std::env::var("DM_TENANT").unwrap_or_else(|_| "default".to_string())
}
