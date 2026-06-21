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

/// True if the agent config already wires SOME memory system (any SessionStart hook), so the
/// wizard can warn before touching it. Conservative: only inspects this config's own hooks.
pub fn has_memory_hooks(config_path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(config_path) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return false };
    v.get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(|s| s.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

/// Merge dm's hooks into a config file's `hooks` key (or, with `remove`, drop them). Idempotent:
/// always drops any prior dm entries (matched by the dm binary path) first.
fn install_into(config_path: &Path, dm: &str, remove: bool) -> Result<()> {
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

    // SessionEnd is intentionally NOT installed: Claude Code forbids context injection on
    // SessionEnd, so the save nudge rides UserPromptSubmit (see hooks.rs). It is still listed
    // here (with None) so a stale SessionEnd hook from an older dmem version is cleaned on
    // re-bootstrap.
    let events: [(&str, Option<Value>); 3] = [
        ("SessionStart", Some(hook_entry(dm, "hook session_start", 10))),
        ("UserPromptSubmit", Some(hook_entry(dm, "hook user_prompt_submit", 8))),
        ("SessionEnd", None),
    ];
    for (event, our_entry) in &events {
        // keep existing entries that are not ours (command does not reference the dm binary)
        let mut kept: Vec<Value> = hooks_obj
            .get(*event)
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
        if !remove {
            if let Some(e) = our_entry {
                kept.extend(e.as_array().unwrap().iter().cloned());
            }
        }
        if kept.is_empty() {
            hooks_obj.remove(*event);
        } else {
            hooks_obj.insert((*event).to_string(), Value::Array(kept));
        }
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(config_path, out).with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

/// Codex: register `dmem mcp` as a stdio MCP server in ~/.codex/config.toml, and drop any stale
/// v1 daimon HTTP-MCP. Format-preserving (toml_edit), backed up to config.toml.dmbak, and the
/// edited document is re-parsed before it overwrites Codex's config so a bad edit can never
/// corrupt it.
fn codex_install(dm: &str, remove: bool) -> Result<()> {
    let cfg = home()?.join(".codex/config.toml");
    if !cfg.exists() {
        println!("  skip Codex (no ~/.codex/config.toml)");
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?;
    let _ = std::fs::write(cfg.with_file_name("config.toml.dmbak"), &raw);
    let mut doc: toml_edit::DocumentMut = raw.parse().with_context(|| "parse ~/.codex/config.toml")?;
    if doc.get("mcp_servers").and_then(|s| s.as_table()).is_none() {
        doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let servers = doc["mcp_servers"].as_table_mut().unwrap();
    servers.remove("dmem"); // idempotent re-run
    servers.remove("daimon"); // migrate off the v1 HTTP MCP
    if !remove {
        let mut t = toml_edit::Table::new();
        t["command"] = toml_edit::value(dm);
        let mut args = toml_edit::Array::new();
        args.push("mcp");
        t["args"] = toml_edit::value(args);
        servers["dmem"] = toml_edit::Item::Table(t);
    }
    let out = doc.to_string();
    out.parse::<toml_edit::DocumentMut>().with_context(|| "refusing to write: edited config.toml no longer parses")?;
    std::fs::write(&cfg, out).with_context(|| format!("write {}", cfg.display()))?;
    println!(
        "  {} Codex MCP -> {} (mcp_servers.dmem = `dmem mcp`)",
        if remove { "unwired" } else { "wired" },
        cfg.display()
    );
    if !remove {
        println!("    note: Codex auto-recall hooks (persona/recall) land in the next build; the MCP save/recall tools work now.");
    }
    Ok(())
}

pub fn run(devin: bool, claude: bool) -> Result<()> {
    run_mode(devin, claude, false, false)
}

/// Wire or (with `remove`) unwire dmem into the selected agents. Devin + Claude Code use the
/// generic Claude-compatible settings.json hook merge; Codex uses a bespoke `~/.codex/config.toml`
/// MCP installer (more harnesses land here next).
pub fn run_mode(devin: bool, claude: bool, codex: bool, remove: bool) -> Result<()> {
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
        install_into(path, &dm, remove)?;
        println!("  {} {} -> {}", if remove { "unwired" } else { "wired" }, name, path.display());
        did_any = true;
    }

    if codex {
        codex_install(&dm, remove)?;
        did_any = true;
    }

    if !did_any {
        println!("Nothing changed. Pass --devin / --claude / --codex (or --all), and ensure the agent is installed.");
        return Ok(());
    }
    println!();
    if remove {
        println!("Done. dmem hooks removed (the agent's other hooks/plugins are untouched).");
    } else {
        println!("Done. dmem is wired in (SessionStart -> persona/recent, UserPromptSubmit -> recall + save nudge).");
        println!("Undo any time with:  dmem bootstrap --remove --devin / --claude");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_remove_restores() {
        let dir = std::env::temp_dir().join(format!("dmboot-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // pre-existing UNRELATED hook must survive everything
        std::fs::write(
            &cfg,
            r#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/other/tool x"}]}]}}"#,
        )
        .unwrap();

        install_into(&cfg, "/path/to/dmem", false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // we install SessionStart + UserPromptSubmit; SessionEnd is intentionally NOT wired
        assert!(v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook user_prompt_submit"));
        assert!(v["hooks"].get("SessionEnd").is_none(), "SessionEnd must not be installed");
        // the unrelated hook + our hook both present
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);

        // idempotent re-run: still one dm entry
        install_into(&cfg, "/path/to/dmem", false).unwrap();
        let v2: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v2["hooks"]["SessionStart"].as_array().unwrap().len(), 2);

        // remove: our hooks gone, the unrelated one stays, empty events dropped
        install_into(&cfg, "/path/to/dmem", true).unwrap();
        let v3: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v3["hooks"].get("SessionEnd").is_none(), "dm-only event removed");
        assert_eq!(v3["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(v3["hooks"]["SessionStart"][0]["hooks"][0]["command"], "/other/tool x");
    }

    #[test]
    fn install_cleans_stale_session_end_from_older_versions() {
        let dir = std::env::temp_dir().join(format!("dmboot2-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // an older dmem wired a SessionEnd hook; a re-bootstrap must drop it (CC rejects it)
        std::fs::write(
            &cfg,
            r#"{"hooks":{"SessionEnd":[{"matcher":"","hooks":[{"type":"command","command":"/path/to/dmem hook session_end","timeout":8}]}]}}"#,
        )
        .unwrap();
        install_into(&cfg, "/path/to/dmem", false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v["hooks"].get("SessionEnd").is_none(), "stale dmem SessionEnd must be cleaned");
        assert!(v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook user_prompt_submit"));
    }
}
