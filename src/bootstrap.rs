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
/// always drops any prior dm entries (matched by the dm binary path) first. Returns false when
/// the existing file is not strict JSON: these are the user's LIVE settings (permissions, env,
/// other tools' hooks) - replacing an unparseable file with `{}` destroys all of it, so refuse
/// and let the caller say so. A parseable file is backed up to `<file>.dmbak` before rewrite.
fn install_into(config_path: &Path, dm: &str, remove: bool) -> Result<bool> {
    let mut root: Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        }
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    if config_path.exists() {
        let _ = std::fs::copy(config_path, PathBuf::from(format!("{}.dmbak", config_path.display())));
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
    Ok(true)
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

/// Wire (or with `remove`, unwire) the dmem stdio MCP server through a CLI agent's own `mcp`
/// subcommand (`claude mcp add` / `devin mcp add`), so the save tools register the canonical way
/// - the same pattern as `codex plugin add`. Idempotent (drops any prior entry first). Returns
/// true if wired (or cleanly removed); false if the agent CLI is missing or the add failed, so
/// the caller can print the manual command. Best-effort: never aborts the whole bootstrap.
fn agent_mcp(cli: &str, add_args: &[&str], rm_args: &[&str], remove: bool) -> bool {
    let _ = std::process::Command::new(cli).args(rm_args).output();
    if remove {
        return true;
    }
    matches!(std::process::Command::new(cli).args(add_args).output(), Ok(o) if o.status.success())
}

/// Claude Desktop has NO hook system, only MCP, so wiring it means adding an `mcpServers.dmem`
/// entry to its claude_desktop_config.json (it then reads `initialize.instructions` for persona +
/// protocols and exposes the bootstrap/recall prompts). Idempotent: drops any prior `dmem` entry
/// first and preserves every other mcpServers entry. Path resolved via `dirs::config_dir()`: macOS
/// `~/Library/Application Support/Claude`, Windows `%APPDATA%\Claude`, Linux `~/.config/Claude`.
fn claude_desktop_install(dm: &str, remove: bool) -> Result<()> {
    let Some(path) = dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json")) else {
        println!("  skip Claude Desktop (no config dir on this OS)");
        return Ok(());
    };
    let dir_present = path.parent().map(|p| p.exists()).unwrap_or(false);
    if !dir_present && !path.exists() {
        println!("  skip Claude Desktop (not installed; no {})", path.parent().map(|p| p.display().to_string()).unwrap_or_default());
        return Ok(());
    }
    let existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                // Users hand-edit this file for MCP servers; replacing an unparseable copy
                // with only our entry would drop every other server. Refuse instead.
                println!("  skip Claude Desktop -> {} is not valid JSON ({e}); fix it and re-run (nothing was changed).", path.display());
                return Ok(());
            }
        }
    } else {
        json!({})
    };
    if path.exists() {
        let _ = std::fs::copy(&path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    let root = desktop_merge(existing, dm, remove);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    if remove {
        println!("  unwired Claude Desktop -> {} (removed mcpServers.dmem)", path.display());
    } else {
        println!("  wired Claude Desktop -> {} (MCP tools + persona via instructions + bootstrap prompt)", path.display());
        println!("    quit and relaunch Claude Desktop to load it (it reads this config at launch; no hot reload).");
    }
    Ok(())
}

/// Pure JSON merge for Claude Desktop's config: ensure `mcpServers` is an object, drop any prior
/// `dmem` entry (idempotent), and on `!remove` add `{command: dm, args: ["mcp"]}`. Every other
/// key (and every other mcpServers entry) is preserved untouched. A non-object input is replaced
/// with a fresh object, matching `install_into`'s defensive handling of a malformed config.
fn desktop_merge(mut root: Value, dm: &str, remove: bool) -> Value {
    if !root.is_object() {
        root = json!({});
    }
    let servers = root.as_object_mut().unwrap().entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    let servers = servers.as_object_mut().unwrap();
    servers.remove("dmem");
    if !remove {
        servers.insert("dmem".to_string(), json!({ "command": dm, "args": ["mcp"] }));
    }
    root
}

/// The OpenCode plugin source written by `dmem bootstrap --opencode`. `__DMEM__` is replaced
/// with a JSON-escaped absolute path to this binary (a valid JS string literal, so paths with
/// spaces/quotes/backslashes survive). OpenCode loads single-file plugins from the global config
/// dir at startup; every hook body is try/catch'd and every dmem call is raced against a hard
/// timeout, so the plugin can never block or crash the host. `experimental.chat.system.transform`
/// receives no prompt text, so `chat.message` stashes the latest user text per session first.
const OPENCODE_PLUGIN_TS: &str = r#"// dmem OpenCode plugin - generated by `dmem bootstrap --opencode`; re-run that command to
// refresh this file, or `dmem bootstrap --opencode --remove` to unwire. Injects the dmem
// persona/protocols and per-prompt recalled memory into the system prompt, and surfaces a
// save-discipline nudge when the session goes idle. Every dmem call has a hard timeout and
// falls back to empty output, so a slow or missing dmem can never block or crash OpenCode.

const DMEM = __DMEM__; // absolute dmem path, substituted by `dmem bootstrap --opencode`
const TIMEOUT_MS = 8000;
const NUDGE_COOLDOWN_MS = 15 * 60_000;

export const DmemPlugin = async ({ client }: any) => {
  // The file can be discovered twice (config `plugin` entry + directory scan); register once.
  const g = globalThis as any;
  if (g.__dmemPluginLoaded) return {};
  g.__dmemPluginLoaded = true;

  let persona: string | null = null; // cached for the plugin lifetime
  const lastPrompt = new Map<string, string>(); // sessionID -> latest user text
  let lastRecall: { key: string; text: string } | null = null; // transform fires twice per turn
  let lastNudgeAt = 0;

  // Shell out to dmem via Bun.spawn: argv reaches the child verbatim (no shell parsing, so
  // no quote-wrapping surprises), the prompt rides stdin, and the timeout KILLS the child -
  // a wedged dmem (server unreachable, cold-start download) must not accumulate processes
  // across a long session. Empty string on ANY failure.
  const dmem = async (args: string[], stdin?: string): Promise<string> => {
    try {
      const proc = Bun.spawn([DMEM, ...args], {
        stdin: stdin === undefined ? "ignore" : new Response(stdin),
        stdout: "pipe",
        stderr: "ignore",
      });
      const timer = setTimeout(() => {
        try { proc.kill(); } catch {}
      }, TIMEOUT_MS);
      const out = await new Response(proc.stdout).text();
      const code = await proc.exited;
      clearTimeout(timer);
      return code === 0 ? out.trim() : "";
    } catch {
      return "";
    }
  };

  return {
    // Stash the latest user text per session; the system transform below receives no prompt.
    "chat.message": async (input: any, output: any) => {
      try {
        const sid = input?.sessionID ?? output?.message?.sessionID;
        const text = (output?.parts ?? [])
          .filter((p: any) => p?.type === "text" && !p?.synthetic)
          .map((p: any) => p.text)
          .join("\n")
          .trim();
        if (sid && text) {
          // refresh insertion order, then evict the oldest session past the cap - the stash
          // must not grow without bound across a long multi-session TUI run
          lastPrompt.delete(sid);
          lastPrompt.set(sid, text);
          if (lastPrompt.size > 64) lastPrompt.delete(lastPrompt.keys().next().value);
        }
      } catch {}
    },

    // Persona + protocols (fetched once, cached) and per-prompt recall -> system prompt.
    "experimental.chat.system.transform": async (input: any, output: any) => {
      try {
        // cache only a NON-EMPTY persona: a failed first fetch (dmem still starting) must
        // retry on later turns, not stick as permanently absent for the process lifetime
        if (!persona) persona = (await dmem(["hook", "session_start", "--raw"])) || null;
        if (persona) output.system.push(persona);
        const sid = input?.sessionID;
        const prompt = sid ? lastPrompt.get(sid) : undefined;
        if (prompt) {
          // The prompt travels as the stdin JSON payload (the Claude-Code hook shape dmem
          // already parses), never argv. Memoized: the transform also fires for auxiliary
          // model calls (e.g. title generation) within the same turn.
          const key = sid + " " + prompt;
          const recall =
            lastRecall?.key === key
              ? lastRecall.text
              : await dmem(["hook", "user_prompt_submit", "--raw"], JSON.stringify({ prompt }));
          lastRecall = { key, text: recall };
          if (recall) output.system.push(recall);
        }
      } catch {}
    },

    // Save-discipline nudge on idle: only when dmem says work looks uncaptured, rate-limited.
    event: async ({ event }: any) => {
      try {
        if (event?.type !== "session.idle") return;
        const now = Date.now();
        if (now - lastNudgeAt < NUDGE_COOLDOWN_MS) return;
        const nudge = await dmem(["hook", "session_end", "--raw"]);
        if (!nudge) return;
        lastNudgeAt = now;
        await client.tui.showToast({
          body: {
            message: "dmem: this session looks uncaptured - save decisions/lessons (dmem tools) before moving on",
            variant: "warning",
          },
        });
      } catch {}
    },
  };
};
"#;

/// Substitute the dmem binary path into the plugin source as a JS string literal (JSON string
/// escaping is valid JS, so quotes/backslashes in the path cannot break out of the literal).
fn opencode_plugin_src(dm: &str) -> Result<String> {
    let lit = serde_json::to_string(dm)?;
    Ok(OPENCODE_PLUGIN_TS.replace("__DMEM__", &lit))
}

/// Pure JSON merge for OpenCode's global config: an `mcp.dmem` entry (local stdio server;
/// `command` is an ARRAY in OpenCode's schema, unlike Claude Desktop's string+args) and a
/// `plugin` array entry pointing at the written plugin file. Idempotent: prior dmem entries
/// are dropped first; every other key, mcp server, and plugin entry is preserved. A non-object
/// root is replaced with a fresh object, matching `desktop_merge`.
fn opencode_merge(mut root: Value, dm: &str, plugin_url: &str, remove: bool) -> Value {
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let servers = obj.entry("mcp").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    let servers = servers.as_object_mut().unwrap();
    servers.remove("dmem");
    if !remove {
        servers.insert(
            "dmem".to_string(),
            json!({ "type": "local", "command": [dm, "mcp"], "enabled": true }),
        );
    }
    let plugins = obj.entry("plugin").or_insert_with(|| json!([]));
    if !plugins.is_array() {
        *plugins = json!([]);
    }
    let arr = plugins.as_array_mut().unwrap();
    arr.retain(|p| p.as_str().map(|s| !s.ends_with("/dmem.ts")).unwrap_or(true));
    if !remove {
        arr.push(json!(plugin_url));
    }
    if arr.is_empty() {
        obj.remove("plugin");
    }
    root
}

/// Read-modify-write OpenCode's config with comment safety. OpenCode accepts JSONC, so a config
/// that fails strict JSON parsing (comments / trailing commas) is REFUSED rather than rewritten:
/// a lossy rewrite would destroy the user's comments. (Contrast `install_into`, whose configs
/// are machine-written strict JSON.) Returns true if the file was edited; false = refused, the
/// caller prints a paste-ready snippet. A missing file is created; an existing one is backed up
/// to `<file>.dmbak` first.
fn opencode_write_config(path: &Path, dm: &str, plugin_url: &str, remove: bool) -> Result<bool> {
    let existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        }
    } else {
        json!({})
    };
    if path.exists() {
        let _ = std::fs::copy(path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    let root = opencode_merge(existing, dm, plugin_url, remove);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Wire (or with `remove`, unwire) OpenCode: a single-file TypeScript plugin (persona + recall
/// into the system prompt, save nudge on session.idle) plus an `mcp.dmem` entry for the save
/// tools - the same stdio MCP server Claude Desktop uses. The plugin is registered both ways
/// OpenCode discovers plugins (config `plugin` entry AND the scanned plugin directory), with an
/// in-plugin load guard so double discovery registers once.
fn opencode_install(dm: &str, remove: bool) -> Result<()> {
    let dir = home()?.join(".config/opencode");
    if !dir.exists() {
        println!("  skip OpenCode (no {})", dir.display());
        return Ok(());
    }
    // Prefer the plugin dir that already exists (OpenCode scans both plugin/ and plugins/);
    // clean the sibling name on every run so a stale copy never lingers.
    let plug_dir = if dir.join("plugins").exists() { dir.join("plugins") } else { dir.join("plugin") };
    let stale = if plug_dir.ends_with("plugins") { dir.join("plugin/dmem.ts") } else { dir.join("plugins/dmem.ts") };
    let _ = std::fs::remove_file(&stale);
    let plug_file = plug_dir.join("dmem.ts");
    if remove {
        let _ = std::fs::remove_file(&plug_file);
    } else {
        std::fs::create_dir_all(&plug_dir)?;
        std::fs::write(&plug_file, opencode_plugin_src(dm)?)
            .with_context(|| format!("write {}", plug_file.display()))?;
    }
    let plugin_url = format!("file://{}", plug_file.display());
    // Config target: the highest-precedence existing global file (OpenCode's own preference
    // order: opencode.jsonc > opencode.json > config.json); else create opencode.json.
    let cfg = ["opencode.jsonc", "opencode.json", "config.json"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.exists())
        .unwrap_or_else(|| dir.join("opencode.json"));
    let edited = opencode_write_config(&cfg, dm, &plugin_url, remove)?;
    if remove {
        if edited {
            println!("  unwired OpenCode -> {} (removed the dmem plugin + mcp.dmem)", cfg.display());
        } else {
            println!("  removed the OpenCode dmem plugin file; {} was NOT edited (not strict JSON) - delete its mcp.dmem and dmem.ts plugin entries manually.", cfg.display());
        }
    } else if edited {
        println!("  wired OpenCode -> {} (plugin: persona + recall + idle nudge; MCP save tools)", cfg.display());
        println!("    restart opencode to load the plugin (plugins load at startup).");
    } else {
        println!("  wrote {} but LEFT {} untouched (it isn't strict JSON; rewriting would lose comments).", plug_file.display(), cfg.display());
        println!("    add these entries to it manually, then restart opencode:");
        println!("      \"mcp\":    {{ \"dmem\": {{ \"type\": \"local\", \"command\": [{}, \"mcp\"], \"enabled\": true }} }}", serde_json::to_string(dm)?);
        println!("      \"plugin\": [{}]", serde_json::to_string(&plugin_url)?);
    }
    Ok(())
}

/// Grok CLI: wire dmem as an MCP server via `grok mcp add` - the canonical route, same pattern
/// as claude/devin. MCP ONLY, deliberately: Grok v0 hooks are observe/block-only - its embedded
/// hook docs state that for passive events (SessionStart etc.) "stdout is ignored", and no
/// context-injection output field exists (verified against Grok CLI 0.2.99: the binary carries
/// `decision`/`systemMessage` output keys but no `additionalContext` equivalent). A CC-style
/// hook plugin would therefore spawn dmem every prompt and have its persona/recall output
/// silently discarded. Until Grok grows context injection, persona rides the MCP server's
/// `initialize.instructions` (hosts that surface it) and recall is tool-driven.
fn grok_install(dm: &str, remove: bool) -> Result<()> {
    let grok_dir = home()?.join(".grok");
    if !grok_dir.exists() {
        println!("  skip Grok (no ~/.grok - install it first: https://x.ai/cli)");
        return Ok(());
    }
    // Clean up the hook-plugin experiment from pre-release builds of this target.
    let stale_plug = grok_dir.join("dmem-plugin");
    if stale_plug.exists() {
        let _ = std::process::Command::new("grok").args(["plugin", "uninstall", "dmem"]).output();
        let _ = std::fs::remove_dir_all(&stale_plug);
    }
    let mcp_ok = agent_mcp(
        "grok",
        &["mcp", "add", "dmem", "--", dm, "mcp"],
        &["mcp", "remove", "dmem"],
        remove,
    );
    if remove {
        println!("  unwired Grok (MCP entry removed)");
    } else if mcp_ok {
        println!("  wired Grok -> ~/.grok/config.toml (MCP: recall/remember/log_* tools)");
        println!("    note: Grok v0 hooks cannot inject context (passive-hook stdout is ignored),");
        println!("    so there is no per-prompt auto-recall here - the model recalls via the MCP tools.");
    } else {
        println!("  Grok MCP step failed; run manually:  grok mcp add dmem -- {dm} mcp");
    }
    Ok(())
}

pub fn run(devin: bool, claude: bool, codex: bool, hermes: bool, opencode: bool, grok: bool, claude_desktop: bool) -> Result<()> {
    run_mode(devin, claude, codex, hermes, opencode, grok, claude_desktop, false)
}

/// Wire or (with `remove`) unwire dmem into the selected agents. Devin + Claude Code use the
/// generic Claude-compatible settings.json hook merge; Codex uses a bespoke `~/.codex/config.toml`
/// MCP+plugin installer; Hermes uses a `~/.hermes/config.yaml` MCP+shell-hook installer; OpenCode
/// gets a TypeScript plugin + `mcp.dmem` entry in ~/.config/opencode; Grok gets an MCP entry via
/// `grok mcp add` (its v0 hooks cannot inject context, so no hook plugin); Claude Desktop
/// (hook-less) gets an MCP entry in claude_desktop_config.json.
pub fn run_mode(devin: bool, claude: bool, codex: bool, hermes: bool, opencode: bool, grok: bool, claude_desktop: bool, remove: bool) -> Result<()> {
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
        if !install_into(path, &dm, remove)? {
            println!("  skip {} -> {} is not valid JSON; fix it and re-run (nothing was changed).", name, path.display());
            continue;
        }
        // Parity with codex/hermes: also wire the MCP save tools via the agent's own `mcp` CLI.
        // Hooks alone give persona + recall; the remember/log_* tools come from the MCP server.
        let (cli, add, rm): (&str, Vec<&str>, Vec<&str>) = if i == 0 {
            ("devin", vec!["mcp", "add", "dmem", "--", dm.as_str(), "mcp"], vec!["mcp", "remove", "dmem"])
        } else {
            ("claude", vec!["mcp", "add", "dmem", "--scope", "user", "--", dm.as_str(), "mcp"], vec!["mcp", "remove", "dmem", "--scope", "user"])
        };
        let mcp_ok = agent_mcp(cli, &add, &rm, remove);
        if remove {
            println!("  unwired {} -> {} (hooks + MCP)", name, path.display());
        } else if mcp_ok {
            println!("  wired {} -> {} (hooks + MCP save tools)", name, path.display());
        } else {
            println!("  wired {} -> {} (hooks only). MCP step failed; run manually: {} {}", name, path.display(), cli, add.join(" "));
        }
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

    if opencode {
        opencode_install(&dm, remove)?;
        did_any = true;
    }

    if grok {
        grok_install(&dm, remove)?;
        did_any = true;
    }

    if claude_desktop {
        claude_desktop_install(&dm, remove)?;
        did_any = true;
    }

    if !did_any {
        println!("Nothing changed. Pass --devin / --claude / --codex / --hermes / --opencode / --grok / --claude-desktop (or --all), and ensure the agent is installed.");
        return Ok(());
    }
    println!();
    if remove {
        println!("Done. dmem hooks removed (the agent's other hooks/plugins are untouched).");
    } else {
        println!("Done. dmem is wired in (SessionStart -> persona/recent, UserPromptSubmit -> recall + save nudge).");
        println!("Undo any time with:  dmem bootstrap --remove --devin / --claude / --codex / --hermes / --opencode / --grok / --claude-desktop");
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
    fn desktop_merge_adds_dmem_preserves_others_and_is_idempotent() {
        // a config with an unrelated MCP server and a sibling top-level key
        let base = serde_json::json!({
            "mcpServers": { "other": { "command": "x", "args": [] } },
            "globalShortcut": "Cmd+K"
        });
        let wired = desktop_merge(base.clone(), "/abs/dmem", false);
        assert_eq!(wired["mcpServers"]["dmem"]["command"], "/abs/dmem");
        assert_eq!(wired["mcpServers"]["dmem"]["args"][0], "mcp");
        assert_eq!(wired["mcpServers"]["other"]["command"], "x", "unrelated server preserved");
        assert_eq!(wired["globalShortcut"], "Cmd+K", "unrelated top-level key preserved");
        // idempotent: re-wiring yields exactly one dmem entry
        let again = desktop_merge(wired.clone(), "/abs/dmem", false);
        assert_eq!(again["mcpServers"]["dmem"]["command"], "/abs/dmem");
        // remove drops only dmem, keeps the rest
        let removed = desktop_merge(again, "/abs/dmem", true);
        assert!(removed["mcpServers"].get("dmem").is_none(), "dmem removed");
        assert_eq!(removed["mcpServers"]["other"]["command"], "x", "other server still there");
        assert_eq!(removed["globalShortcut"], "Cmd+K");
        // a malformed (non-object) config becomes a clean object with our entry
        let from_garbage = desktop_merge(serde_json::json!("not an object"), "/abs/dmem", false);
        assert_eq!(from_garbage["mcpServers"]["dmem"]["command"], "/abs/dmem");
    }

    #[test]
    fn install_refuses_unparseable_config_and_backs_up_valid_ones() {
        let dir = std::env::temp_dir().join(format!("dmboot3-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // a trailing comma (the classic hand-edit slip) must refuse, byte-identical
        let broken = "{ \"permissions\": { \"allow\": [\"Bash(git *)\",] } }";
        std::fs::write(&cfg, broken).unwrap();
        assert!(!install_into(&cfg, "/path/to/dmem", false).unwrap(), "unparseable config refused");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), broken, "refused file untouched");
        // a valid config is backed up before the rewrite and its keys survive
        std::fs::write(&cfg, r#"{"env":{"KEEP":"1"}}"#).unwrap();
        assert!(install_into(&cfg, "/path/to/dmem", false).unwrap());
        assert!(dir.join("config.json.dmbak").exists(), "backup written before rewrite");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["env"]["KEEP"], "1", "unrelated keys preserved");
    }

    #[test]
    fn opencode_merge_wires_mcp_and_plugin_preserves_others_and_is_idempotent() {
        // a real-world shape: unrelated mcp server, existing plugin entries, sibling keys
        let base = serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": { "other": { "type": "local", "command": ["x"], "enabled": true } },
            "plugin": ["file:///home/u/.config/opencode/plugins/attyx-status.js", "opencode-claude-auth@latest"],
            "shell": "zsh"
        });
        let url = "file:///home/u/.config/opencode/plugins/dmem.ts";
        let wired = opencode_merge(base.clone(), "/abs/dmem", url, false);
        assert_eq!(wired["mcp"]["dmem"]["type"], "local");
        assert_eq!(wired["mcp"]["dmem"]["command"][0], "/abs/dmem", "command is an ARRAY (OpenCode schema)");
        assert_eq!(wired["mcp"]["dmem"]["command"][1], "mcp");
        assert_eq!(wired["mcp"]["dmem"]["enabled"], true);
        assert_eq!(wired["mcp"]["other"]["command"][0], "x", "unrelated server preserved");
        assert_eq!(wired["plugin"].as_array().unwrap().len(), 3, "our entry appended");
        assert_eq!(wired["plugin"][2], url);
        assert_eq!(wired["$schema"], "https://opencode.ai/config.json");
        assert_eq!(wired["shell"], "zsh");
        // idempotent: re-wiring keeps exactly one dmem plugin entry and one mcp entry
        let again = opencode_merge(wired.clone(), "/abs/dmem", url, false);
        assert_eq!(again["plugin"].as_array().unwrap().len(), 3);
        // remove drops only ours; the other plugins and servers stay
        let removed = opencode_merge(again, "/abs/dmem", url, true);
        assert!(removed["mcp"].get("dmem").is_none(), "mcp.dmem removed");
        assert_eq!(removed["mcp"]["other"]["command"][0], "x");
        assert_eq!(removed["plugin"].as_array().unwrap().len(), 2, "only our plugin entry removed");
        // garbage root -> clean wired object; empty plugin array is dropped on remove
        let from_garbage = opencode_merge(serde_json::json!("nope"), "/abs/dmem", url, false);
        assert_eq!(from_garbage["mcp"]["dmem"]["command"][0], "/abs/dmem");
        let bare_removed = opencode_merge(serde_json::json!({}), "/abs/dmem", url, true);
        assert!(bare_removed.get("plugin").is_none(), "no empty plugin array left behind");
    }

    #[test]
    fn opencode_plugin_src_substitutes_an_escaped_js_literal() {
        let src = opencode_plugin_src("/plain/path/dmem").unwrap();
        assert!(!src.contains("__DMEM__"), "placeholder fully substituted");
        assert!(src.contains(r#"const DMEM = "/plain/path/dmem";"#));
        // the hook names the host discovers us by must be present verbatim
        for needle in ["experimental.chat.system.transform", "chat.message", "session.idle"] {
            assert!(src.contains(needle), "missing hook name {needle}");
        }
        // hostile path characters stay inside the string literal
        let tricky = opencode_plugin_src(r#"/spa ced/qu"ote/back\slash/dmem"#).unwrap();
        assert!(tricky.contains(r#"const DMEM = "/spa ced/qu\"ote/back\\slash/dmem";"#));
    }

    #[test]
    fn opencode_write_config_refuses_jsonc_but_creates_missing() {
        let dir = std::env::temp_dir().join(format!("dmoc-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        // a commented (JSONC) config must be refused and left byte-identical
        let jsonc = dir.join("opencode.jsonc");
        let original = "// my precious comments\n{ \"shell\": \"zsh\" }\n";
        std::fs::write(&jsonc, original).unwrap();
        let edited = opencode_write_config(&jsonc, "/abs/dmem", "file:///p/dmem.ts", false).unwrap();
        assert!(!edited, "JSONC config must be refused");
        assert_eq!(std::fs::read_to_string(&jsonc).unwrap(), original, "refused file untouched");
        // a missing config is created with our entries
        let fresh = dir.join("opencode.json");
        let edited = opencode_write_config(&fresh, "/abs/dmem", "file:///p/dmem.ts", false).unwrap();
        assert!(edited);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&fresh).unwrap()).unwrap();
        assert_eq!(v["mcp"]["dmem"]["command"][0], "/abs/dmem");
        assert_eq!(v["plugin"][0], "file:///p/dmem.ts");
        // an existing strict-JSON config gets a .dmbak backup before rewrite
        let edited = opencode_write_config(&fresh, "/abs/dmem", "file:///p/dmem.ts", true).unwrap();
        assert!(edited);
        assert!(dir.join("opencode.json.dmbak").exists(), "backup written");
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
