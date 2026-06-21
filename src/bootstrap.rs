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

/// Ensure `doc[key]` is a table (create an empty one if it is missing or a non-table).
fn ensure_table(doc: &mut toml_edit::DocumentMut, key: &str) {
    if doc.get(key).and_then(|x| x.as_table()).is_none() {
        doc[key] = toml_edit::Item::Table(toml_edit::Table::new());
    }
}

/// UTC RFC3339 timestamp without pulling in chrono - civil-date-from-days (H. Hinnant). Used for
/// the marketplace `last_updated` field so Codex sees the same shape it writes itself.
fn rfc3339_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Write the dm-lite Codex plugin tree (a local marketplace) whose hooks call the dmem binary.
/// Codex shares Claude Code's hook output shape (hookSpecificOutput.additionalContext), so the
/// same `dmem hook ...` commands drive persona on SessionStart and recall on UserPromptSubmit.
fn codex_write_plugin(mp_dir: &Path, dm: &str) -> Result<()> {
    let plug = mp_dir.join("plugins/dmem");
    std::fs::create_dir_all(mp_dir.join(".claude-plugin"))?;
    std::fs::create_dir_all(plug.join(".codex-plugin"))?;
    std::fs::create_dir_all(plug.join("hooks"))?;
    let market = json!({ "name": "dmem", "plugins": [ { "name": "dmem", "source": "./plugins/dmem" } ] });
    std::fs::write(mp_dir.join(".claude-plugin/marketplace.json"), serde_json::to_string_pretty(&market)? + "\n")?;
    let manifest = json!({
        "name": "dmem",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Shared cross-tool memory for Codex, backed by dm-lite (dmem). Persona + recent context on session start, deterministic hybrid recall per prompt, and remember/recall memory tools.",
        "license": "MIT",
        "hooks": "./hooks/hooks.json"
    });
    std::fs::write(plug.join(".codex-plugin/plugin.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    let hooks = json!({
        "hooks": {
            "SessionStart": [ { "matcher": "*", "hooks": [
                { "type": "command", "command": format!("{dm} hook session_start"), "timeout": 10 } ] } ],
            "UserPromptSubmit": [ { "matcher": "*", "hooks": [
                { "type": "command", "command": format!("{dm} hook user_prompt_submit"), "timeout": 8 } ] } ]
        }
    });
    std::fs::write(plug.join("hooks/hooks.json"), serde_json::to_string_pretty(&hooks)? + "\n")?;
    Ok(())
}

/// Codex: wire dmem as both an MCP server (tools) AND a hook plugin (persona + auto-recall) in
/// ~/.codex/config.toml, and migrate off the v1 daimon-memory marketplace/plugin/HTTP-MCP.
/// Format-preserving (toml_edit), backed up to config.toml.dmbak, and the edited document is
/// re-parsed before it overwrites Codex's config so a bad edit can never corrupt it. Trust hashes
/// are intentionally NOT forged: Codex prompts the user once to trust the hooks on first run.
fn codex_install(dm: &str, remove: bool) -> Result<()> {
    let codex = home()?.join(".codex");
    let cfg = codex.join("config.toml");
    if !cfg.exists() {
        println!("  skip Codex (no ~/.codex/config.toml)");
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?;
    let _ = std::fs::write(cfg.with_file_name("config.toml.dmbak"), &raw);
    let mut doc: toml_edit::DocumentMut = raw.parse().with_context(|| "parse ~/.codex/config.toml")?;
    let mp_dir = codex.join("dmem-marketplace");

    // MCP tools: [mcp_servers.dmem] = `dmem mcp`; drop the v1 HTTP MCP.
    ensure_table(&mut doc, "mcp_servers");
    let servers = doc["mcp_servers"].as_table_mut().unwrap();
    servers.remove("dmem");
    servers.remove("daimon");
    if !remove {
        let mut t = toml_edit::Table::new();
        t["command"] = toml_edit::value(dm);
        let mut args = toml_edit::Array::new();
        args.push("mcp");
        t["args"] = toml_edit::value(args);
        servers["dmem"] = toml_edit::Item::Table(t);
    }

    // Hook plugin: register a local marketplace + enable the plugin; drop the v1 marketplace/plugin.
    ensure_table(&mut doc, "marketplaces");
    let markets = doc["marketplaces"].as_table_mut().unwrap();
    markets.remove("daimon-memory");
    markets.remove("dmem");
    if !remove {
        let mut t = toml_edit::Table::new();
        t["source_type"] = toml_edit::value("local");
        t["source"] = toml_edit::value(mp_dir.to_string_lossy().as_ref());
        t["last_updated"] = toml_edit::value(rfc3339_utc());
        markets["dmem"] = toml_edit::Item::Table(t);
    }
    ensure_table(&mut doc, "plugins");
    let plugins = doc["plugins"].as_table_mut().unwrap();
    plugins.remove("daimon-memory@daimon-memory");
    plugins.remove("dmem@dmem");
    if !remove {
        let mut t = toml_edit::Table::new();
        t["enabled"] = toml_edit::value(true);
        plugins["dmem@dmem"] = toml_edit::Item::Table(t);
        ensure_table(&mut doc, "features");
        doc["features"]["plugin_hooks"] = toml_edit::value(true);
    }

    // Drop the v1 plugin's hook trust records so Codex does not keep stale daimon-memory state.
    if let Some(state) = doc.get_mut("hooks").and_then(|h| h.get_mut("state")).and_then(|s| s.as_table_mut()) {
        let stale: Vec<String> = state.iter().map(|(k, _)| k.to_string()).filter(|k| k.starts_with("daimon-memory@")).collect();
        for k in stale {
            state.remove(&k);
        }
    }

    let out = doc.to_string();
    out.parse::<toml_edit::DocumentMut>().with_context(|| "refusing to write: edited config.toml no longer parses")?;
    std::fs::write(&cfg, out).with_context(|| format!("write {}", cfg.display()))?;

    if remove {
        // Let Codex clean its own plugin cache + state, then drop the local marketplace source.
        let _ = std::process::Command::new("codex").args(["plugin", "remove", "dmem@dmem"]).output();
        let _ = std::fs::remove_dir_all(&mp_dir);
        println!("  unwired Codex (MCP + hook plugin) -> {}", cfg.display());
    } else {
        codex_write_plugin(&mp_dir, dm)?;
        // The config + marketplace source are not enough: Codex loads hook plugins from its
        // install CACHE (~/.codex/plugins/cache/<mp>/<plugin>/<version>/), populated only by
        // `codex plugin add`. Without this the plugin is "not installed" and no hooks fire.
        match std::process::Command::new("codex").args(["plugin", "add", "dmem@dmem"]).output() {
            Ok(o) if o.status.success() => {
                println!("  wired Codex -> {} (MCP tools + dmem hook plugin, installed into Codex's cache)", cfg.display());
            }
            Ok(o) => {
                println!("  wired Codex config -> {} (MCP tools + dmem hook plugin source)", cfg.display());
                println!("    warn: `codex plugin add dmem@dmem` failed ({}). Run it manually to install the hooks.", String::from_utf8_lossy(&o.stderr).trim());
            }
            Err(_) => {
                println!("  wired Codex config -> {} (MCP tools + dmem hook plugin source)", cfg.display());
                println!("    NOTE: `codex` CLI not found - run `codex plugin add dmem@dmem` to install the hook plugin into Codex's cache.");
            }
        }
        println!("    On your next Codex session, Codex asks once to TRUST the dmem hooks");
        println!("    (session_start + user_prompt_submit). Accept to enable persona + auto-recall.");
    }
    Ok(())
}

/// serde_yaml emits YAML 1.2, but Hermes loads its config with PyYAML (1.1), where the bare
/// tokens off/on/yes/no/y/n are booleans. serde only ever emits those bare for STRING values
/// (1.2 keeps them strings), so any such bare value in our output is a string PyYAML would
/// silently misread (e.g. `mode: 'off'` round-tripped to `mode: off` -> False). Re-single-quote
/// exactly those scalar values; keys and already-quoted/structured values are left untouched.
/// This keeps the structural round-trip (which handles every config shape) safe for PyYAML.
fn yaml_quote_pyyaml_unsafe(yaml: &str) -> String {
    fn risky(v: &str) -> bool {
        matches!(v.to_ascii_lowercase().as_str(), "y" | "n" | "yes" | "no" | "on" | "off")
    }
    let mut out = String::with_capacity(yaml.len() + 16);
    for piece in yaml.split_inclusive('\n') {
        let nl = piece.ends_with('\n');
        let line = piece.trim_end_matches('\n');
        let indent_len = line.len() - line.trim_start().len();
        let (indent, rest) = line.split_at(indent_len);
        let fixed = if let Some(pos) = rest.find(": ") {
            let (k, v) = (&rest[..pos], rest[pos + 2..].trim());
            if risky(v) { Some(format!("{indent}{k}: '{v}'")) } else { None }
        } else if let Some(v) = rest.strip_prefix("- ") {
            let v = v.trim();
            if risky(v) { Some(format!("{indent}- '{v}'")) } else { None }
        } else {
            None
        };
        out.push_str(fixed.as_deref().unwrap_or(line));
        if nl {
            out.push('\n');
        }
    }
    out
}

/// Merge (or, with `remove`, drop) a single scoped approval for our hook command into Hermes's
/// shell-hooks allowlist. This is deliberately narrow - we allowlist ONLY `dmem`'s own command
/// rather than flipping the global `hooks_auto_accept`, so dmem's hook registers without a TTY
/// prompt while every other shell hook still requires the user's explicit consent.
fn hermes_allowlist(hook_cmd: &str, remove: bool) -> Result<()> {
    let path = home()?.join(".hermes/shell-hooks-allowlist.json");
    let mut doc: Value = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({ "approvals": [] }))
    } else {
        json!({ "approvals": [] })
    };
    if !doc.get("approvals").map(|a| a.is_array()).unwrap_or(false) {
        doc["approvals"] = json!([]);
    }
    let approvals = doc["approvals"].as_array_mut().unwrap();
    approvals.retain(|e| e.get("command").and_then(|c| c.as_str()) != Some(hook_cmd));
    if !remove {
        approvals.push(json!({ "event": "pre_llm_call", "command": hook_cmd }));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&doc)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Markers fencing the dmem-managed block inside Hermes's SOUL.md. Anything outside them is the
/// user's own content and is preserved across re-syncs.
const SOUL_BEGIN: &str = "<!-- BEGIN dmem-managed: persona + protocols (source of truth = your daimon memory; refresh with `dmem bootstrap --hermes`) -->";
const SOUL_END: &str = "<!-- END dmem-managed -->";
/// Lead-in so the model treats the projected protocols as binding operating rules (not just
/// personality/tone, which is SOUL.md's default framing) and knows the exact save tools.
const SOUL_LEAD: &str = "You ARE the persona defined below, and the protocols below are your binding operating rules, not style notes. The Memory Save Discipline governs WHEN and HOW you persist memory; the Behavioral Discipline governs how you work. Persist durable memory with your memory tools (this harness exposes them as mcp_dmem_recall, mcp_dmem_remember, mcp_dmem_log_decision, mcp_dmem_log_lesson, mcp_dmem_log_incident, mcp_dmem_add_reminder, mcp_dmem_forget) exactly as the Memory Save Discipline directs.";

/// Project the live dmem persona + protocols into a dmem-managed block in Hermes's SOUL.md (its
/// always-on system-prompt identity), so Izu's identity + governance are present on every message
/// - fresh session, resumed session, or after compaction - which the per-prompt user-message hook
/// cannot guarantee. Recent/recalled memory stays on the hook. Content OUTSIDE the markers (the
/// user's own SOUL.md edits) is preserved. SOUL.md is reloaded by Hermes each message (no restart).
fn hermes_sync_soul(remove: bool) -> Result<()> {
    let soul = home()?.join(".hermes/SOUL.md");
    let existing = std::fs::read_to_string(&soul).unwrap_or_default();
    // Drop any prior dmem-managed block, keep everything else verbatim.
    let outside = match (existing.find(SOUL_BEGIN), existing.find(SOUL_END)) {
        (Some(b), Some(e)) if e > b => {
            let before = existing[..b].trim_end();
            let after = existing[e + SOUL_END.len()..].trim_start_matches('\n');
            if after.is_empty() {
                before.to_string()
            } else {
                format!("{before}\n{after}")
            }
        }
        _ => existing.trim_end().to_string(),
    };
    let new_content = if remove {
        if outside.is_empty() { String::new() } else { format!("{outside}\n") }
    } else {
        let m = crate::tools::Memory::open().map_err(|e| anyhow!("open memory: {e:#}"))?;
        let persona = m.persona().map_err(|e| anyhow!("read persona: {e:#}"))?;
        if persona.is_empty() {
            return Err(anyhow!("no persona/protocol records to project (seed them first, e.g. `dmem setup`)"));
        }
        let block = crate::render::render_soul(&persona);
        if outside.is_empty() {
            format!("{SOUL_BEGIN}\n{SOUL_LEAD}\n\n{block}\n{SOUL_END}\n")
        } else {
            format!("{outside}\n\n{SOUL_BEGIN}\n{SOUL_LEAD}\n\n{block}\n{SOUL_END}\n")
        }
    };
    if let Some(p) = soul.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&soul, new_content).with_context(|| format!("write {}", soul.display()))?;
    Ok(())
}

/// Hermes: wire dmem as an MCP server (tools) + a `pre_llm_call` shell hook (recall every turn),
/// project persona/protocols into SOUL.md (always-on identity), allowlist just that one hook
/// command, and migrate off the v1 daimon memory provider. Backed up to config.yaml.dmbak; the
/// edited YAML is re-parsed before it overwrites the config.
fn hermes_install(dm: &str, remove: bool) -> Result<()> {
    use serde_yaml_ng::{Mapping, Value as Y};
    let cfg = home()?.join(".hermes/config.yaml");
    if !cfg.exists() {
        println!("  skip Hermes (no ~/.hermes/config.yaml)");
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?;
    let _ = std::fs::write(cfg.with_file_name("config.yaml.dmbak"), &raw);
    let mut doc: Y = serde_yaml_ng::from_str(&raw).with_context(|| "parse ~/.hermes/config.yaml")?;
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("~/.hermes/config.yaml is not a YAML mapping"))?;
    let hook_cmd = format!("{dm} hook user_prompt_submit --hermes");

    // MCP tools: mcp_servers.dmem = { command, args:[mcp] }; drop the v1 daimon server.
    let mcp = root
        .entry(Y::from("mcp_servers"))
        .or_insert_with(|| Y::Mapping(Mapping::new()));
    if let Some(m) = mcp.as_mapping_mut() {
        m.remove("dmem");
        m.remove("daimon");
        if !remove {
            let mut e = Mapping::new();
            e.insert(Y::from("command"), Y::from(dm));
            e.insert(Y::from("args"), Y::Sequence(vec![Y::from("mcp")]));
            m.insert(Y::from("dmem"), Y::Mapping(e));
        }
    }

    // Hook: hooks.pre_llm_call - keep any non-dmem entries, (re)add ours.
    let hooks = root
        .entry(Y::from("hooks"))
        .or_insert_with(|| Y::Mapping(Mapping::new()));
    if let Some(h) = hooks.as_mapping_mut() {
        let mut kept: Vec<Y> = h
            .get("pre_llm_call")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter(|e| {
                        !e.get("command")
                            .and_then(|c| c.as_str())
                            .map(|c| c.contains(dm))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !remove {
            let mut e = Mapping::new();
            e.insert(Y::from("command"), Y::from(hook_cmd.as_str()));
            e.insert(Y::from("timeout"), Y::from(8));
            kept.push(Y::Mapping(e));
        }
        if kept.is_empty() {
            h.remove("pre_llm_call");
        } else {
            h.insert(Y::from("pre_llm_call"), Y::Sequence(kept));
        }
    }

    // Migrate off v1: only when memory.provider is exactly "daimon" (do not touch other setups).
    if !remove {
        if let Some(prov) = root
            .get_mut("memory")
            .and_then(|m| m.as_mapping_mut())
            .and_then(|m| m.get_mut("provider"))
        {
            if prov.as_str() == Some("daimon") {
                *prov = Y::from("");
            }
        }
    }

    let out = yaml_quote_pyyaml_unsafe(&serde_yaml_ng::to_string(&doc).with_context(|| "serialize ~/.hermes/config.yaml")?);
    serde_yaml_ng::from_str::<Y>(&out).with_context(|| "refusing to write: edited config.yaml no longer parses")?;
    std::fs::write(&cfg, out).with_context(|| format!("write {}", cfg.display()))?;

    hermes_allowlist(&hook_cmd, remove)?;
    let soul_status = match hermes_sync_soul(remove) {
        Ok(()) if remove => "removed the dmem-managed block from ~/.hermes/SOUL.md".to_string(),
        Ok(()) => "persona + protocols -> ~/.hermes/SOUL.md (always-on identity; reloaded each message)".to_string(),
        Err(e) => format!("SOUL.md persona projection skipped ({e:#})"),
    };

    if remove {
        println!("  unwired Hermes (MCP + pre_llm_call hook) -> {}", cfg.display());
        println!("    {soul_status}");
    } else {
        println!("  wired Hermes -> {} (MCP tools + pre_llm_call recall; persona via SOUL.md)", cfg.display());
        println!("    {soul_status}");
        println!("    allowlisted only the dmem hook in ~/.hermes/shell-hooks-allowlist.json (no global auto-accept).");
        println!("    migrated memory.provider off the v1 daimon plugin (set to unset) where it was 'daimon'.");
        println!("    note: SOUL.md is a projection of your dmem persona/protocols; re-run `dmem bootstrap --hermes` after you change them.");
        println!("    restart Hermes once after wiring so it registers the recall hook (it hot-reloads MCP, but registers shell hooks only at startup).");
    }
    Ok(())
}

pub fn run(devin: bool, claude: bool, codex: bool, hermes: bool) -> Result<()> {
    run_mode(devin, claude, codex, hermes, false)
}

/// Wire or (with `remove`) unwire dmem into the selected agents. Devin + Claude Code use the
/// generic Claude-compatible settings.json hook merge; Codex uses a bespoke `~/.codex/config.toml`
/// MCP+plugin installer; Hermes uses a `~/.hermes/config.yaml` MCP+shell-hook installer.
pub fn run_mode(devin: bool, claude: bool, codex: bool, hermes: bool, remove: bool) -> Result<()> {
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

    if hermes {
        hermes_install(&dm, remove)?;
        did_any = true;
    }

    if !did_any {
        println!("Nothing changed. Pass --devin / --claude / --codex / --hermes (or --all), and ensure the agent is installed.");
        return Ok(());
    }
    println!();
    if remove {
        println!("Done. dmem hooks removed (the agent's other hooks/plugins are untouched).");
    } else {
        println!("Done. dmem is wired in (SessionStart -> persona/recent, UserPromptSubmit -> recall + save nudge).");
        println!("Undo any time with:  dmem bootstrap --remove --devin / --claude / --codex / --hermes");
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
