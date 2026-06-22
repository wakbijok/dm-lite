//! `dmem service` - run the local `dmem serve` daemon as a managed OS service so neither you nor
//! a user ever touches raw launchctl/systemctl. macOS -> a LaunchAgent; Linux -> a systemd
//! `--user` unit. `install` writes the unit AND the [server] block in config.toml so the client
//! connects to the same daemon with the same token. The model's RAM is reclaimed by `stop`
//! (in-process eviction does not return it to the OS - see the managed-service decision).

use crate::config;
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;
use std::process::Command;

/// Service name: launchd Label + systemd unit name.
const NAME: &str = "dmem";

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))
}

fn dmem_bin() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

fn gen_token() -> String {
    let mut b = [0u8; 20];
    let _ = getrandom::getrandom(&mut b);
    let mut s = String::from("dmem_");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn run_ok(cmd: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(cmd).args(args).status().map_err(|e| anyhow!("run {cmd}: {e}"))?;
    if !st.success() {
        bail!("`{cmd} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Install + start the daemon as a login service and point config.toml at it. Reuses the token
/// already in config.toml [server] (so existing clients keep working), else mints a fresh one.
pub fn install(addr: &str) -> Result<()> {
    let tenant = config::canonical_tenant(&config::tenant());
    let token = config::server_link().map(|s| s.token.clone()).unwrap_or_else(gen_token);
    // The token is interpolated into a launchd plist / systemd unit (not just TOML), so reject
    // anything outside the opaque-token charset before it can break or inject into those files.
    if !config::is_safe_token(&token) {
        bail!("refusing to install: the configured server token has characters unsafe for unit files; re-issue it with `dmem admin add` / `dmem login`");
    }
    let data_dir = config::data_dir()?.to_string_lossy().into_owned();
    platform::install(addr, &token, &tenant, &data_dir)?;
    write_server_config(addr, &token, &tenant)?;
    let cfg = config::config_path().map(|p| p.display().to_string()).unwrap_or_default();
    println!("dmem: service '{NAME}' installed + started -> `dmem serve` on {addr}, tenant '{tenant}'");
    println!("  clients connect via the [server] block in {cfg}");
    println!("  manage it:  dmem service status | stop | start | restart | uninstall");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    platform::uninstall()?;
    println!("dmem: service '{NAME}' stopped + removed (model RAM reclaimed).");
    println!("  config.toml [server] is left intact; reinstall with `dmem service install`.");
    Ok(())
}

pub fn start() -> Result<()> {
    platform::start()?;
    println!("dmem: service '{NAME}' started.");
    Ok(())
}

pub fn stop() -> Result<()> {
    platform::stop()?;
    println!("dmem: service '{NAME}' stopped (the daemon's ~model RAM is returned to the OS).");
    Ok(())
}

pub fn restart() -> Result<()> {
    platform::restart()?;
    println!("dmem: service '{NAME}' restarted.");
    Ok(())
}

pub fn status() -> Result<()> {
    platform::status()
}

/// Ensure config.toml has a [server] block pointing at the local daemon (so every dmem invocation
/// - cli, hooks, mcp - becomes a thin client of it). 0600 (it holds the bearer token). Built via
/// the toml serializer so the token/url are escaped, not string-interpolated.
fn write_server_config(addr: &str, token: &str, tenant: &str) -> Result<()> {
    let path = config::config_path().ok_or_else(|| anyhow!("no config path"))?;
    let mut doc = toml::Table::new();
    doc.insert("tenant".into(), toml::Value::String(tenant.to_string()));
    let mut server = toml::Table::new();
    server.insert("url".into(), toml::Value::String(format!("http://{addr}")));
    server.insert("token".into(), toml::Value::String(token.to_string()));
    doc.insert("server".into(), toml::Value::Table(server));
    config::write_secret(&path, &toml::to_string(&doc)?)?;
    Ok(())
}

// ---------- macOS (launchd LaunchAgent) ----------
#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist() -> Result<PathBuf> {
        Ok(home()?.join("Library/LaunchAgents").join(format!("{NAME}.plist")))
    }

    /// Escape a value for an XML text node (the plist is XML). Paths and tokens go into
    /// `<string>` nodes, so a stray `&`/`<`/`"` would otherwise corrupt the plist.
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    pub fn install(addr: &str, token: &str, tenant: &str, data_dir: &str) -> Result<()> {
        let bin = xml_escape(&dmem_bin()?);
        let p = plist()?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = xml_escape(&home()?.join("Library/Logs/dmem-serve.log").display().to_string());
        // tenant is already canonical ([a-z0-9_-]); uppercased it stays a safe env-var key.
        let tenant_env = format!("DM_TOKEN_{}", tenant.to_uppercase());
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             \x20 <key>Label</key><string>{name}</string>\n\
             \x20 <key>ProgramArguments</key><array>\n\
             \x20   <string>{bin}</string><string>serve</string><string>--addr</string><string>{addr}</string>\n\
             \x20 </array>\n\
             \x20 <key>EnvironmentVariables</key><dict>\n\
             \x20   <key>{tenant_env}</key><string>{token}</string>\n\
             \x20   <key>DM_DATA_DIR</key><string>{data_dir}</string>\n\
             \x20 </dict>\n\
             \x20 <key>RunAtLoad</key><true/>\n\
             \x20 <key>KeepAlive</key><true/>\n\
             \x20 <key>StandardOutPath</key><string>{log}</string>\n\
             \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
             </dict></plist>\n",
            name = NAME, bin = bin, addr = xml_escape(addr), tenant_env = tenant_env, token = xml_escape(token), data_dir = xml_escape(data_dir), log = log
        );
        std::fs::write(&p, content)?;
        let ps = p.to_string_lossy().into_owned();
        let _ = Command::new("launchctl").args(["unload", &ps]).output();
        run_ok("launchctl", &["load", "-w", &ps])
    }

    pub fn uninstall() -> Result<()> {
        let p = plist()?;
        let _ = Command::new("launchctl").args(["unload", &p.to_string_lossy()]).output();
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }

    pub fn start() -> Result<()> {
        run_ok("launchctl", &["load", "-w", &plist()?.to_string_lossy()])
    }

    pub fn stop() -> Result<()> {
        run_ok("launchctl", &["unload", &plist()?.to_string_lossy()])
    }

    pub fn restart() -> Result<()> {
        let _ = stop();
        start()
    }

    pub fn status() -> Result<()> {
        let out = Command::new("launchctl").args(["list", NAME]).output().map_err(|e| anyhow!("launchctl: {e}"))?;
        if out.status.success() {
            println!("dmem service '{NAME}': RUNNING (launchd)");
        } else {
            println!("dmem service '{NAME}': stopped (not loaded). start with `dmem service start`");
        }
        Ok(())
    }
}

// ---------- Linux (systemd --user unit) ----------
#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit() -> Result<PathBuf> {
        Ok(home()?.join(".config/systemd/user").join(format!("{NAME}.service")))
    }

    pub fn install(addr: &str, token: &str, tenant: &str, data_dir: &str) -> Result<()> {
        let bin = dmem_bin()?;
        let u = unit()?;
        if let Some(parent) = u.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // tenant is canonical; token is validated to the opaque-token charset upstream, so neither
        // can carry a space/newline. Quote the path-bearing values (bin, data_dir) so a path with
        // a space does not split the ExecStart args or the Environment assignment.
        let tenant_env = format!("DM_TOKEN_{}", tenant.to_uppercase());
        let content = format!(
            "[Unit]\nDescription=dmem memory server\nAfter=network.target\n\n\
             [Service]\nExecStart=\"{bin}\" serve --addr {addr}\n\
             Environment={tenant_env}={token}\nEnvironment=\"DM_DATA_DIR={data_dir}\"\nRestart=on-failure\n\n\
             [Install]\nWantedBy=default.target\n",
            bin = bin, addr = addr, tenant_env = tenant_env, token = token, data_dir = data_dir
        );
        std::fs::write(&u, content)?;
        run_ok("systemctl", &["--user", "daemon-reload"])?;
        run_ok("systemctl", &["--user", "enable", "--now", NAME])
    }

    pub fn uninstall() -> Result<()> {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", NAME]).output();
        let u = unit()?;
        if u.exists() {
            std::fs::remove_file(&u)?;
        }
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        Ok(())
    }

    pub fn start() -> Result<()> {
        run_ok("systemctl", &["--user", "start", NAME])
    }

    pub fn stop() -> Result<()> {
        run_ok("systemctl", &["--user", "stop", NAME])
    }

    pub fn restart() -> Result<()> {
        run_ok("systemctl", &["--user", "restart", NAME])
    }

    pub fn status() -> Result<()> {
        run_ok("systemctl", &["--user", "status", NAME, "--no-pager"])
    }
}

// ---------- other OSes ----------
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;
    fn unsupported() -> Result<()> {
        bail!("`dmem service` supports macOS (launchd) and Linux (systemd); on this OS run `dmem serve` directly or use your platform's service manager")
    }
    pub fn install(_a: &str, _t: &str, _n: &str, _d: &str) -> Result<()> { unsupported() }
    pub fn uninstall() -> Result<()> { unsupported() }
    pub fn start() -> Result<()> { unsupported() }
    pub fn stop() -> Result<()> { unsupported() }
    pub fn restart() -> Result<()> { unsupported() }
    pub fn status() -> Result<()> { unsupported() }
}
