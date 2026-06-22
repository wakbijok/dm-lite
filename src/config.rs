//! Paths, config file, and tenant resolution. The model is client/server: a `[server]` block puts
//! the binary in remote-client mode (it talks to a `dmem serve`, local loopback or remote; server
//! mode selects the tenant per request). Without one it falls back to the deprecated embedded mode
//! (one local tenant). One database file per tenant.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The on-disk config (`~/.config/dmem/config.toml`, or `$DM_CONFIG`). All fields optional;
/// an absent file means the deprecated embedded fallback with defaults (prefer a `[server]` block).
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub data_dir: Option<String>,
    pub tenant: Option<String>,
    /// Presence of this block puts the binary in remote-client mode.
    pub server: Option<ServerLink>,
}

/// How a client reaches a remote `dmem serve`. Fields are read by the remote-client path.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "client"), allow(dead_code))]
pub struct ServerLink {
    pub url: String,
    pub token: String,
    /// Accept a self-signed / invalid TLS cert (for trusted networks).
    #[serde(default)]
    pub insecure: bool,
    /// Trust a specific CA / self-signed cert (PEM path) instead of the system roots.
    pub ca_cert: Option<String>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Path to the config file: `$DM_CONFIG`, else `<config-dir>/dmem/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DM_CONFIG") {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("dmem").join("config.toml"))
}

/// The loaded config (cached). Returns defaults if the file is absent or unparseable.
pub fn config() -> &'static Config {
    CONFIG.get_or_init(|| {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| match toml::from_str::<Config>(&s) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("dmem: ignoring bad config ({e})");
                    None
                }
            })
            .unwrap_or_default()
    })
}

/// The remote server link, if the config selects remote-client mode.
#[cfg_attr(not(feature = "client"), allow(dead_code))]
pub fn server_link() -> Option<&'static ServerLink> {
    config().server.as_ref()
}

/// Base data dir: $DM_DATA_DIR, else config `data_dir`, else ~/.local/share/dm, else ~/.dm.
pub fn data_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("DM_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = &config().data_dir {
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

/// Resolve the active tenant: $DM_TENANT, else config `tenant`, else "default".
pub fn tenant() -> String {
    std::env::var("DM_TENANT")
        .ok()
        .or_else(|| config().tenant.clone())
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_block() {
        let cfg: Config = toml::from_str(
            "tenant = \"acme\"\n[server]\nurl = \"https://x\"\ntoken = \"t\"\ninsecure = true\n",
        )
        .unwrap();
        assert_eq!(cfg.tenant.as_deref(), Some("acme"));
        let s = cfg.server.unwrap();
        assert_eq!(s.url, "https://x");
        assert!(s.insecure && s.ca_cert.is_none());
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.server.is_none() && cfg.tenant.is_none() && cfg.data_dir.is_none());
    }
}
