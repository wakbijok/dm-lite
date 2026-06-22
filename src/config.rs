//! Paths, config file, and tenant resolution. The model is client/server: a `[server]` block puts
//! the binary in remote-client mode (it talks to a `dmem serve`, local loopback or remote; server
//! mode selects the tenant per request). Without one it falls back to the deprecated embedded mode
//! (one local tenant). One database file per tenant.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
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

/// The loaded config (cached). An ABSENT/unreadable file is fine (the embedded fallback with
/// defaults). A file that is PRESENT but invalid TOML is fatal: silently falling back to embedded
/// would route reads/writes to the wrong (local) store while the user believes they are connected
/// to a server. We never echo the file body in the error (it may hold a bearer token); only the
/// parse position is reported.
pub fn config() -> &'static Config {
    CONFIG.get_or_init(load_config)
}

fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Config::default(), // absent or unreadable -> embedded fallback
    };
    match toml::from_str::<Config>(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "dmem: config at {} is present but not valid TOML (at {}). Refusing to fall back \
                 to local storage; fix it or re-run `dmem login` / `dmem setup`.",
                path.display(),
                parse_position(&raw, &e)
            );
            std::process::exit(1);
        }
    }
}

/// Line/column of a TOML parse error, computed from the byte span WITHOUT printing any file
/// content (the file may contain a bearer token).
fn parse_position(raw: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let upto = &raw[..span.start.min(raw.len())];
            let line = upto.bytes().filter(|&b| b == b'\n').count() + 1;
            let col = upto.len() - upto.rfind('\n').map(|i| i + 1).unwrap_or(0) + 1;
            format!("line {line}, column {col}")
        }
        None => "unknown position".to_string(),
    }
}

/// A token is safe to interpolate into config/unit files if it is non-empty and limited to the
/// opaque-token charset dmem mints (`dmem_<hex>`) plus `-`. Rejects quotes, whitespace, and
/// control characters that could break TOML/plist/systemd or inject extra directives.
pub fn is_safe_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Write a file that holds a secret (bearer token) with 0600 from the start. Writes to a sibling
/// temp file opened 0600, then atomically renames over the target, so there is no window where the
/// secret is readable at the default umask (the previous write-then-chmod had a TOCTOU gap).
pub fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "secret".into());
        let tmp = path.with_file_name(format!(".{name}.tmp.{}", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all().ok();
        std::fs::rename(&tmp, path)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
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

    #[test]
    fn is_safe_token_rejects_injection_chars() {
        assert!(is_safe_token("dmem_0123abcdef"));
        assert!(is_safe_token("tenant-laptop_1"));
        assert!(!is_safe_token(""), "empty is not safe");
        assert!(!is_safe_token("has space"));
        assert!(!is_safe_token("has\"quote"));
        assert!(!is_safe_token("has\nnewline"));
        assert!(!is_safe_token("has=equals"));
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_is_0600_and_creates_parents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dmcfg-{}-{}", std::process::id(), crate::entry::now_ms()));
        let p = dir.join("nested").join("secret.toml");
        write_secret(&p, "token = \"x\"\n").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a secret-bearing file must be 0600");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "token = \"x\"\n");
        // overwriting an existing (looser) file still ends at 0600
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret(&p, "token = \"y\"\n").unwrap();
        let mode2 = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o600, "rewrite must re-tighten to 0600");
    }
}
