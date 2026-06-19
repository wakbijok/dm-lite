//! `dm bootstrap` - detect agents and install dm's lifecycle hooks into their config,
//! idempotently. Claude-Code-compatible hook format works for both Devin and Claude Code.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))
}

fn dm_bin() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().to_string())
}

/// One CC-compatible hook entry array for an event, calling `dm <subcmd>`.
fn hook_entry(dm: &str, subcmd: &str, timeout: u64) -> Value {
    json!([{
        "matcher": "",
        "hooks": [{ "type": "command", "command": format!("{} {}", dm, subcmd), "timeout": timeout }]
    }])
}

/// Merge dm's hooks into a config file's `hooks` key. Idempotent: drops any prior dm
/// entries (matched by the dm binary path in the command) before adding ours.
fn install_into(config_path: &Path, dm: &str) -> Result<()> {
    let mut root: Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        if raw.contains("//") || raw.contains("/*") {
            eprintln!("  warn: {} may contain comments; they could be lost on rewrite", config_path.display());
        }
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks_obj = hooks.as_object_mut().unwrap();

    let events = [
        ("SessionStart", hook_entry(dm, "hook session_start", 10)),
        ("UserPromptSubmit", hook_entry(dm, "hook user_prompt_submit", 8)),
        ("SessionEnd", hook_entry(dm, "hook session_end", 8)),
    ];
    for (event, our_entry) in events {
        // keep existing entries that are not ours (command does not reference the dm binary)
        let mut kept: Vec<Value> = hooks_obj
            .get(event)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|e| {
                        !e.get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hs| hs.iter().any(|x| {
                                x.get("command").and_then(|c| c.as_str()).map(|c| c.contains(dm)).unwrap_or(false)
                            }))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        kept.extend(our_entry.as_array().unwrap().iter().cloned());
        hooks_obj.insert(event.to_string(), Value::Array(kept));
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(config_path, out).with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

pub fn run(devin: bool, claude: bool) -> Result<()> {
    let dm = dm_bin()?;
    let h = home()?;
    let mut did_any = false;

    let targets: Vec<(&str, PathBuf)> = vec![
        ("Devin CLI", h.join(".config/devin/config.json")),
        ("Claude Code", h.join(".claude/settings.json")),
    ];
    for (i, (name, path)) in targets.iter().enumerate() {
        let want = (i == 0 && devin) || (i == 1 && claude);
        if !want {
            continue;
        }
        let dir_present = path.parent().map(|p| p.exists()).unwrap_or(false);
        if !dir_present && !path.exists() {
            println!("  skip {} (no {} found)", name, path.parent().map(|p| p.display().to_string()).unwrap_or_default());
            continue;
        }
        install_into(path, &dm)?;
        println!("  wired {} -> {}", name, path.display());
        did_any = true;
    }

    if did_any {
        println!();
        println!("Done. dmem is wired in (SessionStart -> persona/recent, UserPromptSubmit -> recall, SessionEnd -> save nudge).");
        println!("Binary: {}", dm);
        println!("Test on Devin: start a devin session; the first prompt should surface a <daimon-memory> block.");
        println!("Seed a memory first, e.g.:  dmem log_decision --title \"hello\" --decision \"it works\"");
    } else {
        println!("Nothing wired. Pass --devin and/or --claude (or --all), and ensure the agent is installed.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_session_end_hook_idempotently() {
        let dir = std::env::temp_dir().join(format!("dmboot-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        install_into(&cfg, "/path/to/dmem").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let cmd = v["hooks"]["SessionEnd"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("hook session_end"), "got: {cmd}");
        // re-run: still exactly one dm-owned entry (idempotent)
        install_into(&cfg, "/path/to/dmem").unwrap();
        let v2: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v2["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
    }
}
