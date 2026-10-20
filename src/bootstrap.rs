//! `dm bootstrap` - detect agents and install dm's lifecycle hooks into their config,
//! idempotently. Claude-Code-compatible hook format works for both Devin and Claude Code, and
//! for the Claude Cowork plugin (Cowork runs the Claude Code engine).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))
}

fn dm_bin() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().to_string())
}

/// Shell-quote the binary path for hook COMMAND STRINGS that hosts run via a shell (audit
/// Medium #7): a path with spaces/`;`/`$()` would otherwise split or inject. Paths from the
/// safe charset pass through unquoted so existing configs stay byte-identical; anything else
/// gets POSIX single-quoting (with the '\'' escape). JSON-array/TOML sites that pass argv
/// directly must keep using the RAW path - do not quote those.
fn sh_quote(path: &str) -> String {
    let safe = path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+'));
    if safe && !path.is_empty() {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// One CC-compatible hook entry array for an event, calling `dm <subcmd>`.
fn hook_entry(dm: &str, subcmd: &str, timeout: u64) -> Value {
    hook_entry_matched(dm, subcmd, timeout, "")
}

/// As `hook_entry`, with an explicit matcher (for SessionStart: `startup|resume|clear|compact`).
fn hook_entry_matched(dm: &str, subcmd: &str, timeout: u64, matcher: &str) -> Value {
    json!([{
        "matcher": matcher,
        "hooks": [{ "type": "command", "command": format!("{} {}", sh_quote(dm), subcmd), "timeout": timeout }]
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
    merge_dm_hooks(&mut root, dm, remove);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(config_path, out).with_context(|| format!("write {}", config_path.display()))?;
    Ok(true)
}

/// The CC-shape hook merge itself, on an in-memory root: drop any prior dm entries (matched by
/// the dm binary path), then (unless `remove`) add ours. Shared by `install_into` and the
/// WorkBuddy settings writer, whose file also carries a non-hook key and must be edited in ONE
/// read-modify-write (two sequential writes would leave `.dmbak` holding an intermediate state,
/// not the pre-run one).
fn merge_dm_hooks(root: &mut Value, dm: &str, remove: bool) {
    if !root.is_object() {
        *root = json!({});
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

/// Codex spills a hook's `additionalContext` to a temp file once it passes an approximate token
/// threshold (2,500 tokens by default, about 10,000 bytes) and injects only a head/tail preview
/// under a "Warning: truncated output" banner. The session-start block (persona + protocols) can
/// exceed that, which silently drops the middle of the protocols from the model's context while
/// the host still reports the hook as run. `additionalContextLimit` on a command handler raises
/// the per-hook threshold (0 disables spilling); Codex honours it for SessionStart and
/// UserPromptSubmit. Sized with headroom over render::SESSION_BUDGET (about 2,300 tokens) so the
/// host cap is never the tighter one; UserPromptSubmit carries a smaller recall block and fires
/// every turn, so it gets a smaller limit.
const CODEX_SESSION_START_CONTEXT_LIMIT: u64 = 8_000;
const CODEX_USER_PROMPT_SUBMIT_CONTEXT_LIMIT: u64 = 4_000;

/// The dm-lite Codex plugin's hooks.json body: both lifecycle hooks call the dmem binary.
fn codex_hooks(dm: &str) -> Value {
    json!({
        "hooks": {
            "SessionStart": [ { "matcher": "*", "hooks": [
                { "type": "command", "command": format!("{} hook session_start", sh_quote(dm)), "timeout": 10,
                  "additionalContextLimit": CODEX_SESSION_START_CONTEXT_LIMIT } ] } ],
            "UserPromptSubmit": [ { "matcher": "*", "hooks": [
                { "type": "command", "command": format!("{} hook user_prompt_submit", sh_quote(dm)), "timeout": 8,
                  "additionalContextLimit": CODEX_USER_PROMPT_SUBMIT_CONTEXT_LIMIT } ] } ]
        }
    })
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
    std::fs::write(plug.join("hooks/hooks.json"), serde_json::to_string_pretty(&codex_hooks(dm))? + "\n")?;
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
        println!("    The same review appears after any dmem upgrade that changes a hook definition:");
        println!("    Codex marks a changed hook as modified and skips it until it is trusted again.");
    }
    Ok(())
}

/// Hermes-agent's home: `~/.hermes` everywhere EXCEPT hermes-on-Windows, which keeps its
/// config under `%LOCALAPPDATA%\hermes` (found 14-07-2026: bootstrap silently skipped on
/// a Windows workstation because ~/.hermes/config.yaml never exists there, so the agent ran with NO
/// shared memory and nobody noticed). Prefer whichever root already holds a config.yaml;
/// fall back to the platform-classic dot-dir so the "skip Hermes" message stays truthful.
pub(crate) fn hermes_dir() -> Result<PathBuf> {
    let dot = home()?.join(".hermes");
    if dot.join("config.yaml").exists() {
        return Ok(dot);
    }
    if let Some(local) = dirs::data_local_dir() {
        let lad = local.join("hermes");
        if lad.join("config.yaml").exists() {
            return Ok(lad);
        }
    }
    Ok(dot)
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
    let path = hermes_dir()?.join("shell-hooks-allowlist.json");
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
/// always-on system-prompt identity), so the agent's identity + governance are present on every message
/// - fresh session, resumed session, or after compaction - which the per-prompt user-message hook
/// cannot guarantee. Recent/recalled memory stays on the hook. Content OUTSIDE the markers (the
/// user's own SOUL.md edits) is preserved. SOUL.md is reloaded by Hermes each message (no restart).
fn hermes_sync_soul(remove: bool) -> Result<()> {
    let soul = hermes_dir()?.join("SOUL.md");
    let block = if remove { None } else { Some(persona_block(SOUL_LEAD)?) };
    upsert_managed_block(&soul, SOUL_BEGIN, SOUL_END, block.as_deref())
}

/// Render the live dmem persona + protocols under a host-specific lead-in. Errors when there is
/// nothing to project so callers can report it instead of writing an empty managed block.
fn persona_block(lead: &str) -> Result<String> {
    let m = crate::tools::Memory::open().map_err(|e| anyhow!("open memory: {e:#}"))?;
    let persona = m.persona().map_err(|e| anyhow!("read persona: {e:#}"))?;
    if persona.is_empty() {
        return Err(anyhow!("no persona/protocol records to project (seed them first, e.g. `dmem setup`)"));
    }
    Ok(format!("{lead}\n\n{}", crate::render::render_soul(&persona)))
}

/// Replace (or with `block: None`, drop) the marker-fenced dmem-managed section of a markdown
/// file, preserving everything outside the markers verbatim. Idempotent: any prior block between
/// the same markers is removed first. A file that would become empty is written empty (not
/// deleted): hosts that re-seed a missing file must not resurrect their stock content over ours.
fn upsert_managed_block(path: &Path, begin: &str, end: &str, block: Option<&str>) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let outside = match (existing.find(begin), existing.find(end)) {
        (Some(b), Some(e)) if e > b => {
            let before = existing[..b].trim_end();
            let after = existing[e + end.len()..].trim_start_matches('\n');
            if after.is_empty() {
                before.to_string()
            } else {
                format!("{before}\n{after}")
            }
        }
        _ => existing.trim_end().to_string(),
    };
    let new_content = match block {
        None => {
            if outside.is_empty() { String::new() } else { format!("{outside}\n") }
        }
        Some(block) => {
            if outside.is_empty() {
                format!("{begin}\n{block}\n{end}\n")
            } else {
                format!("{outside}\n\n{begin}\n{block}\n{end}\n")
            }
        }
    };
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, new_content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Enable `dmem` (and drop leftover v1 `daimon`) on `plugins.enabled`. Desktop
/// user plugins are gated on that list; without this the plugin we install is
/// discovered then skipped.
fn hermes_toggle_plugin_enabled(root: &mut serde_yaml_ng::Mapping, remove: bool) {
    use serde_yaml_ng::{Mapping, Value as Y};
    let plugins = root
        .entry(Y::from("plugins"))
        .or_insert_with(|| Y::Mapping(Mapping::new()));
    let Some(p) = plugins.as_mapping_mut() else {
        return;
    };
    let enabled = p
        .entry(Y::from("enabled"))
        .or_insert_with(|| Y::Sequence(vec![]));
    let Some(seq) = enabled.as_sequence_mut() else {
        return;
    };
    seq.retain(|v| {
        let s = v.as_str().unwrap_or("");
        if s == "dmem" {
            return false;
        }
        // Wiring dmem replaces the v1 daimon memory plugin; do not resurrect it on remove.
        !(s == "daimon" && !remove)
    });
    if !remove {
        seq.push(Y::from("dmem"));
    }
}

/// Install (or remove) `~/.hermes/plugins/dmem` — the desktop-safe pre_llm_call path.
fn hermes_write_plugin(dm: &str, remove: bool) -> Result<()> {
    let dir = hermes_dir()?.join("plugins/dmem");
    if remove {
        let _ = std::fs::remove_file(dir.join("__init__.py"));
        let _ = std::fs::remove_file(dir.join("plugin.yaml"));
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("plugin.yaml"), include_str!("../contrib/hermes-plugin/plugin.yaml"))
        .with_context(|| format!("write {}", dir.join("plugin.yaml").display()))?;
    let bin_lit = serde_json::to_string(dm).context("encode dmem path")?;
    let src = include_str!("../contrib/hermes-plugin/__init__.py").replace("\"__DMEM_BIN__\"", &bin_lit);
    std::fs::write(dir.join("__init__.py"), src)
        .with_context(|| format!("write {}", dir.join("__init__.py").display()))?;
    Ok(())
}

/// Hermes: wire dmem as an MCP server (tools) + a `pre_llm_call` shell hook (recall every turn),
/// project persona/protocols into SOUL.md (always-on identity), allowlist just that one hook
/// command, and migrate off the v1 daimon memory provider. Backed up to config.yaml.dmbak; the
/// edited YAML is re-parsed before it overwrites the config.
fn hermes_install(dm: &str, remove: bool) -> Result<()> {
    use serde_yaml_ng::{Mapping, Value as Y};
    let cfg = hermes_dir()?.join("config.yaml");
    if !cfg.exists() {
        println!("  skip Hermes (no config.yaml in ~/.hermes or the local-appdata hermes dir)");
        return Ok(());
    }
    let raw = std::fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?;
    let _ = std::fs::write(cfg.with_file_name("config.yaml.dmbak"), &raw);
    let mut doc: Y = serde_yaml_ng::from_str(&raw).with_context(|| "parse ~/.hermes/config.yaml")?;
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("~/.hermes/config.yaml is not a YAML mapping"))?;
    let hook_cmd = format!("{} hook user_prompt_submit --hermes", sh_quote(dm));

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

    // Desktop chat (tui_gateway / hermes serve) never calls register_from_config, so the
    // shell hook above is dead there. A user plugin *does* load (AIAgent discover_plugins)
    // and is gated by plugins.enabled. Enable `dmem`, drop leftover v1 `daimon`.
    hermes_toggle_plugin_enabled(root, remove);

    let out = yaml_quote_pyyaml_unsafe(&serde_yaml_ng::to_string(&doc).with_context(|| "serialize ~/.hermes/config.yaml")?);
    serde_yaml_ng::from_str::<Y>(&out).with_context(|| "refusing to write: edited config.yaml no longer parses")?;
    std::fs::write(&cfg, out).with_context(|| format!("write {}", cfg.display()))?;

    hermes_allowlist(&hook_cmd, remove)?;
    hermes_write_plugin(dm, remove)?;
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
        println!("    installed ~/.hermes/plugins/dmem (desktop pre_llm_call; tui_gateway skips shell hooks).");
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

/// Name of the generated Claude Cowork plugin (kebab-case, as Cowork requires).
pub const COWORK_PLUGIN_NAME: &str = "dmem-cowork";

/// Where `dmem bootstrap --cowork` writes the plugin tree and its `.plugin` zip. Cowork installs
/// user plugins only through its Settings -> Plugins upload, so this is a hand-over location, not
/// a config dir; Cowork's own plugin store (rpm/plugin_<id>/) is never written to.
pub fn cowork_root() -> Result<PathBuf> {
    Ok(home()?.join(".daimon").join("cowork-plugin"))
}

/// Cowork plugin manifest. No `hooks` key: the plugin loader discovers `hooks/hooks.json` by its
/// default location, matching the manifests Cowork ships itself. No `mcpServers` either: the
/// tools come from the `mcpServers.dmem` entry in claude_desktop_config.json, and shipping a
/// second copy inside the plugin would give the session a duplicate dmem tool set.
fn cowork_manifest() -> Value {
    json!({
        "name": COWORK_PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Shared cross-tool memory for Claude Cowork, backed by dm-lite (dmem): persona + protocols on session start, deterministic hybrid recall on every prompt. Memory tools come from the dmem MCP server registered in claude_desktop_config.json (dmem bootstrap --claude-desktop).",
        "author": { "name": "daimon" }
    })
}

/// SessionStart sources the Cowork plugin reacts to. Cowork runs one engine query per user turn
/// and resumes the session each time, so `resume` fires on every turn; its output is not attached
/// to the resumed conversation (the persona from `startup` is already in it), so running the hook
/// there is a wasted round-trip. `compact` stays: the persona must be re-injected after a compaction.
const COWORK_SESSION_START_MATCHER: &str = "startup|clear|compact";

/// Cowork runs the Claude Code engine, so plugin hooks share the settings.json hook shape and
/// the default `dmem hook ...` output envelope. Same events and timeouts as `install_into`; no
/// SessionEnd for the same reason (Claude Code forbids context injection there).
fn cowork_hooks(dm: &str) -> Value {
    json!({
        "hooks": {
            "SessionStart": hook_entry_matched(dm, "hook session_start", 10, COWORK_SESSION_START_MATCHER),
            "UserPromptSubmit": hook_entry(dm, "hook user_prompt_submit", 8),
        }
    })
}

/// Write the plugin tree (`.claude-plugin/plugin.json` + `hooks/hooks.json`) at `plug`, recreated
/// from scratch so no stale file survives. Returns the two file bodies for the zip writer.
fn cowork_write_tree(plug: &Path, dm: &str) -> Result<(String, String)> {
    if plug.exists() {
        std::fs::remove_dir_all(plug).with_context(|| format!("clear {}", plug.display()))?;
    }
    std::fs::create_dir_all(plug.join(".claude-plugin"))?;
    std::fs::create_dir_all(plug.join("hooks"))?;
    let manifest = serde_json::to_string_pretty(&cowork_manifest())? + "\n";
    let hooks = serde_json::to_string_pretty(&cowork_hooks(dm))? + "\n";
    std::fs::write(plug.join(".claude-plugin/plugin.json"), &manifest)?;
    std::fs::write(plug.join("hooks/hooks.json"), &hooks)?;
    Ok((manifest, hooks))
}

/// Write the plugin tree under `root/dmem-cowork/` and pack it into `root/dmem-cowork.plugin`
/// (a zip of the tree's contents, entries at the archive root) for the manual Settings -> Plugins
/// upload. The zip is byte-deterministic (stored entries, fixed timestamp), so a re-run that
/// changes nothing yields an identical file.
fn cowork_write_plugin(root: &Path, dm: &str) -> Result<PathBuf> {
    let (manifest, hooks) = cowork_write_tree(&root.join(COWORK_PLUGIN_NAME), dm)?;
    let zip = root.join(format!("{COWORK_PLUGIN_NAME}.plugin"));
    write_stored_zip(
        &[(".claude-plugin/plugin.json", manifest.as_bytes()), ("hooks/hooks.json", hooks.as_bytes())],
        &zip,
    )?;
    Ok(zip)
}

/// Delete the generated plugin tree and zip. Returns true if anything was there to remove.
fn cowork_remove(root: &Path) -> Result<bool> {
    let plug = root.join(COWORK_PLUGIN_NAME);
    let zip = root.join(format!("{COWORK_PLUGIN_NAME}.plugin"));
    let mut existed = false;
    if plug.exists() {
        std::fs::remove_dir_all(&plug).with_context(|| format!("remove {}", plug.display()))?;
        existed = true;
    }
    if zip.exists() {
        std::fs::remove_file(&zip).with_context(|| format!("remove {}", zip.display()))?;
        existed = true;
    }
    Ok(existed)
}

/// CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320) as the ZIP format requires. Bitwise form: the
/// inputs are two small JSON files, so a lookup table would be more code than it saves.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Minimal ZIP writer: every entry stored (method 0), fixed DOS timestamp 1980-01-01 00:00, no
/// data descriptors, no ZIP64, no extra fields. Enough for a plugin of a few small JSON files, and
/// deliberately dependency-free (a compression crate would add a tree of transitive deps for
/// zero benefit on a 1 KB payload) and portable (the tests must not need a system `zip` binary).
/// Entry names are the archive-relative paths with `/` separators, exactly as given.
fn write_stored_zip(entries: &[(&str, &[u8])], out: &Path) -> Result<()> {
    fn u16le(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn u32le(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    const DOS_TIME: u16 = 0x0000; // 00:00:00
    const DOS_DATE: u16 = 0x0021; // 1980-01-01
    let mut body: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for (name, data) in entries {
        let name_b = name.as_bytes();
        let crc = crc32(data);
        let size = u32::try_from(data.len()).map_err(|_| anyhow!("zip entry too large: {name}"))?;
        let offset = u32::try_from(body.len()).map_err(|_| anyhow!("zip too large"))?;
        // local file header
        u32le(&mut body, 0x0403_4b50);
        u16le(&mut body, 10); // version needed: 1.0 (stored)
        u16le(&mut body, 0); // flags
        u16le(&mut body, 0); // method: stored
        u16le(&mut body, DOS_TIME);
        u16le(&mut body, DOS_DATE);
        u32le(&mut body, crc);
        u32le(&mut body, size);
        u32le(&mut body, size);
        u16le(&mut body, name_b.len() as u16);
        u16le(&mut body, 0); // extra len
        body.extend_from_slice(name_b);
        body.extend_from_slice(data);
        // central directory header
        u32le(&mut central, 0x0201_4b50);
        u16le(&mut central, 20); // version made by: 2.0, MS-DOS attrs
        u16le(&mut central, 10); // version needed
        u16le(&mut central, 0); // flags
        u16le(&mut central, 0); // method
        u16le(&mut central, DOS_TIME);
        u16le(&mut central, DOS_DATE);
        u32le(&mut central, crc);
        u32le(&mut central, size);
        u32le(&mut central, size);
        u16le(&mut central, name_b.len() as u16);
        u16le(&mut central, 0); // extra len
        u16le(&mut central, 0); // comment len
        u16le(&mut central, 0); // disk number start
        u16le(&mut central, 0); // internal attrs
        u32le(&mut central, 0); // external attrs
        u32le(&mut central, offset);
        central.extend_from_slice(name_b);
    }
    let cd_offset = u32::try_from(body.len()).map_err(|_| anyhow!("zip too large"))?;
    let cd_size = u32::try_from(central.len()).map_err(|_| anyhow!("zip too large"))?;
    let n = u16::try_from(entries.len()).map_err(|_| anyhow!("too many zip entries"))?;
    let mut eocd: Vec<u8> = Vec::new();
    u32le(&mut eocd, 0x0605_4b50);
    u16le(&mut eocd, 0); // this disk
    u16le(&mut eocd, 0); // disk with central dir
    u16le(&mut eocd, n);
    u16le(&mut eocd, n);
    u32le(&mut eocd, cd_size);
    u32le(&mut eocd, cd_offset);
    u16le(&mut eocd, 0); // comment len
    body.extend_from_slice(&central);
    body.extend_from_slice(&eocd);
    std::fs::write(out, body).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

/// Plugin id as Cowork's registry keys it: `<name>@<marketplace>`. The marketplace half is
/// free-form for a locally installed plugin (only the `org-provisioned` suffix is reserved).
pub const COWORK_PLUGIN_ID: &str = "dmem-cowork@dmem";

/// Cowork workspaces on this machine: one directory per account/org under the app's
/// local-agent-mode-sessions dir. Each holds the workspace's plugin registry and settings. A
/// level-2 dir counts as a workspace when it carries any of Cowork's own markers, which keeps
/// unrelated siblings (e.g. the bundled skills plugin cache) out.
fn cowork_workspaces() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(base) = dirs::config_dir().map(|d| d.join("Claude").join("local-agent-mode-sessions")) else { return out };
    let Ok(accts) = std::fs::read_dir(&base) else { return out };
    for acct in accts.flatten() {
        if !acct.path().is_dir() || acct.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(orgs) = std::fs::read_dir(acct.path()) else { continue };
        for org in orgs.flatten() {
            let p = org.path();
            if !p.is_dir() || org.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let marker = p.join("rpm").is_dir()
                || p.join("agent").is_dir()
                || p.join("spaces.json").is_file()
                || p.join("cowork_settings.json").is_file();
            if marker {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Read a JSON file the app owns. Ok(None) when absent, Err when present but not strict JSON:
/// these are live app files (other plugins, other settings), so an unparseable copy is never
/// replaced - the caller refuses and says so, as `install_into` does for settings.json.
fn read_json_file(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value = serde_json::from_str(&raw).with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(Some(v))
}

fn write_json_file(path: &Path, v: &Value) -> Result<()> {
    if path.exists() {
        let _ = std::fs::copy(path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n").with_context(|| format!("write {}", path.display()))
}

/// Pure merge for a workspace's `cowork_plugins/installed_plugins.json`. Same v2 schema Claude
/// Code uses for `~/.claude/plugins/installed_plugins.json` (Cowork reads it with the same
/// shape): `plugins.<id>` is an array of install entries. Ours is replaced wholesale (idempotent),
/// keeping the original `installedAt`; every other plugin is preserved. `scope` must be `user`
/// for Cowork to load it outside a project.
fn cowork_registry_merge(mut root: Value, install_path: &str, remove: bool, now: &str) -> Value {
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    obj.entry("version").or_insert(json!(2));
    let plugins = obj.entry("plugins").or_insert_with(|| json!({}));
    if !plugins.is_object() {
        *plugins = json!({});
    }
    let plugins = plugins.as_object_mut().unwrap();
    let installed_at = plugins
        .get(COWORK_PLUGIN_ID)
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("installedAt"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| now.to_string());
    plugins.remove(COWORK_PLUGIN_ID);
    if !remove {
        plugins.insert(
            COWORK_PLUGIN_ID.to_string(),
            json!([{
                "scope": "user",
                "installPath": install_path,
                "version": env!("CARGO_PKG_VERSION"),
                "installedAt": installed_at,
                "lastUpdated": now,
            }]),
        );
    }
    root
}

/// Pure merge for a workspace's `cowork_settings.json`: flip `enabledPlugins.<id>` on (or drop it
/// on remove), preserving every other key the app keeps there.
fn cowork_settings_merge(mut root: Value, remove: bool) -> Value {
    if !root.is_object() {
        root = json!({});
    }
    let enabled = root.as_object_mut().unwrap().entry("enabledPlugins").or_insert_with(|| json!({}));
    if !enabled.is_object() {
        *enabled = json!({});
    }
    let enabled = enabled.as_object_mut().unwrap();
    if remove {
        enabled.remove(COWORK_PLUGIN_ID);
    } else {
        enabled.insert(COWORK_PLUGIN_ID.to_string(), json!(true));
    }
    root
}

/// Install (or remove) the plugin directly in one Cowork workspace: copy the tree into
/// `<ws>/cowork_plugins/dmem-cowork/` (Cowork only loads plugins that live inside its plugins dir),
/// register it in `cowork_plugins/installed_plugins.json`, and enable it in `cowork_settings.json`.
/// Cowork's local-plugins reader runs at every session spawn and passes the result to the Claude
/// Code engine as a plugin dir, hooks included. Returns false (nothing changed) when either app
/// file is present but unparseable.
fn cowork_local_install(ws: &Path, dm: &str, remove: bool) -> Result<bool> {
    let plugins_dir = ws.join("cowork_plugins");
    let plug = plugins_dir.join(COWORK_PLUGIN_NAME);
    let registry = plugins_dir.join("installed_plugins.json");
    let settings = ws.join("cowork_settings.json");
    let (reg, set) = match (read_json_file(&registry), read_json_file(&settings)) {
        (Ok(r), Ok(s)) => (r, s),
        (Err(e), _) | (_, Err(e)) => {
            println!("  skip Cowork workspace {} -> {e}; fix it and re-run (nothing was changed).", ws.display());
            return Ok(false);
        }
    };
    let now = rfc3339_utc();
    if remove {
        if plug.exists() {
            std::fs::remove_dir_all(&plug).with_context(|| format!("remove {}", plug.display()))?;
        }
        // only rewrite files that exist; never create app files just to record an absence
        if let Some(r) = reg {
            write_json_file(&registry, &cowork_registry_merge(r, "", true, &now))?;
        }
        if let Some(s) = set {
            write_json_file(&settings, &cowork_settings_merge(s, true))?;
        }
        return Ok(true);
    }
    std::fs::create_dir_all(&plugins_dir)?;
    cowork_write_tree(&plug, dm)?;
    let install_path = plug.to_string_lossy().to_string();
    write_json_file(&registry, &cowork_registry_merge(reg.unwrap_or(json!({})), &install_path, false, &now))?;
    write_json_file(&settings, &cowork_settings_merge(set.unwrap_or(json!({})), false))?;
    Ok(true)
}

/// Claude Cowork (macOS): Cowork runs the Claude Code engine on the host with a per-session
/// config dir, so the hooks `--claude` writes to ~/.claude/settings.json never load there, and
/// the only way in is a plugin. We install one directly into every Cowork workspace found (hooks
/// only; the MCP tools ride the claude_desktop_config.json entry, ensured here unless the caller
/// already wrote it) and also emit a `.plugin` zip as the documented fallback (Claude Desktop ->
/// Settings -> Plugins upload) for machines where no workspace exists yet.
fn cowork_install(dm: &str, remove: bool, desktop_done: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        println!("  skip Claude Cowork (macOS only)");
        return Ok(());
    }
    let root = cowork_root()?;
    if remove {
        let mut n = 0;
        for ws in cowork_workspaces() {
            if cowork_local_install(&ws, dm, true)? {
                n += 1;
            }
        }
        if n > 0 {
            println!("  unwired Claude Cowork in {n} workspace(s) (plugin dir, registry entry and enabled flag removed)");
        }
        if cowork_remove(&root)? {
            println!("  removed the generated Claude Cowork plugin under {}", root.display());
        }
        if n == 0 {
            println!("  Claude Cowork: nothing was installed locally");
        }
        println!("    if you uploaded '{COWORK_PLUGIN_NAME}' by hand, uninstall it in Claude Desktop -> Settings -> Plugins.");
        println!("    mcpServers.dmem in claude_desktop_config.json is left in place (Desktop chat uses it); `--claude-desktop --remove` unwires that.");
        return Ok(());
    }
    let app_present = dirs::config_dir().map(|d| d.join("Claude").exists()).unwrap_or(false);
    if !app_present {
        println!("  skip Claude Cowork (Claude Desktop not installed)");
        return Ok(());
    }
    if !desktop_done {
        claude_desktop_install(dm, false)?;
    }
    let zip = cowork_write_plugin(&root, dm)?;
    let wss = cowork_workspaces();
    let mut n = 0;
    for ws in &wss {
        if cowork_local_install(ws, dm, false)? {
            println!("  wired Claude Cowork -> {} (plugin installed + enabled; SessionStart -> persona/recent, UserPromptSubmit -> recall)", ws.join("cowork_plugins").join(COWORK_PLUGIN_NAME).display());
            n += 1;
        }
    }
    if n > 0 {
        println!("    start a NEW Cowork session to load it (an open session keeps its old plugin set).");
        println!("    the hooks carry this binary's absolute path ({dm}); if it moves, re-run `dmem bootstrap --cowork`.");
    } else {
        if wss.is_empty() {
            println!("  no Cowork workspace found yet (open Cowork once, then re-run `dmem bootstrap --cowork`),");
        }
        println!("  fallback: Claude Desktop -> Settings -> Plugins -> upload {}", zip.display());
    }
    Ok(())
}

/// Best-effort: does any Cowork workspace list our plugin among the ones uploaded through the app
/// (server-synced store)? Read-only. None when no manifest could be read at all.
fn cowork_uploaded_in_app() -> Option<bool> {
    let mut saw_manifest = false;
    for ws in cowork_workspaces() {
        let Ok(raw) = std::fs::read_to_string(ws.join("rpm").join("manifest.json")) else { continue };
        saw_manifest = true;
        let Ok(v) = serde_json::from_str::<Value>(&raw) else { continue };
        let hit = v.get("plugins")
            .and_then(|p| p.as_array())
            .map(|arr| arr.iter().any(|p| p.get("name").and_then(|n| n.as_str()) == Some(COWORK_PLUGIN_NAME)))
            .unwrap_or(false);
        if hit {
            return Some(true);
        }
    }
    if saw_manifest { Some(false) } else { None }
}

/// One-line wiring report for `dmem status`: in how many Cowork workspaces the plugin is installed
/// (registered + enabled + tree present), whether any hooks.json still points at this binary,
/// and whether a hand-uploaded copy exists as well.
pub fn cowork_status() -> String {
    if !cfg!(target_os = "macos") {
        return "n/a (macOS only)".to_string();
    }
    let wss = cowork_workspaces();
    if wss.is_empty() {
        return "no Cowork workspace found (open Cowork once, then `dmem bootstrap --cowork`)".to_string();
    }
    let dm = dm_bin().unwrap_or_default();
    let mut installed = 0;
    let mut stale = false;
    for ws in &wss {
        let registered = read_json_file(&ws.join("cowork_plugins").join("installed_plugins.json"))
            .ok()
            .flatten()
            .and_then(|v| v.get("plugins")?.get(COWORK_PLUGIN_ID).cloned())
            .is_some();
        let enabled = read_json_file(&ws.join("cowork_settings.json"))
            .ok()
            .flatten()
            .and_then(|v| v.get("enabledPlugins")?.get(COWORK_PLUGIN_ID)?.as_bool())
            .unwrap_or(false);
        let hooks = ws.join("cowork_plugins").join(COWORK_PLUGIN_NAME).join("hooks").join("hooks.json");
        let Ok(raw) = std::fs::read_to_string(&hooks) else { continue };
        if registered && enabled {
            installed += 1;
            if !dm.is_empty() && !raw.contains(&dm) {
                stale = true;
            }
        }
    }
    let uploaded = match cowork_uploaded_in_app() {
        Some(true) => "; also uploaded via Settings -> Plugins",
        _ => "",
    };
    if installed == 0 {
        return format!("not wired in {} workspace(s) (run `dmem bootstrap --cowork`){uploaded}", wss.len());
    }
    format!(
        "wired in {installed}/{} workspace(s){}{uploaded}",
        wss.len(),
        if stale { "; STALE binary path, re-run `dmem bootstrap --cowork`" } else { "" }
    )
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

/// Markers fencing the dmem-managed block in WorkBuddy's identity files. Same fence mechanism as
/// Hermes's SOUL.md, its own BEGIN text so the refresh hint names the right flag.
const WB_BEGIN: &str = "<!-- BEGIN dmem-managed: persona + protocols (source of truth = your daimon memory; refresh with `dmem bootstrap --workbuddy`) -->";

/// Lead-in for WorkBuddy's SOUL.md block. WorkBuddy injects SOUL.md into context EVERY turn and
/// its stock templates invite the agent to edit the identity files and build MEMORY.md stores -
/// exactly the drift this projection exists to prevent, so the lead names the dmem tools (CC-style
/// `mcp__dmem__*` here, vs Hermes's `mcp_dmem_*`) and forbids persisting through the files.
const WB_SOUL_LEAD: &str = "You ARE the persona defined below, and the protocols below are your binding operating rules, not style notes. The Memory Save Discipline governs WHEN and HOW you persist memory; the Behavioral Discipline governs how you work. Durable memory lives ONLY in daimon memory (dmem): persist with the dmem MCP tools (this harness exposes them as mcp__dmem__recall, mcp__dmem__remember, mcp__dmem__log_decision, mcp__dmem__log_lesson, mcp__dmem__log_incident, mcp__dmem__add_reminder, mcp__dmem__forget) exactly as the Memory Save Discipline directs - never by editing SOUL.md/IDENTITY.md/USER.md and never by writing MEMORY.md memory files.";

const WB_IDENTITY_NOTE: &str = "Identity source of truth is daimon memory (dmem) - see the dmem-managed block in SOUL.md. Keep this file a thin pointer: do not accumulate identity changes here; update the dmem persona records instead.";

const WB_USER_NOTE: &str = "The user profile lives in daimon memory (dmem). Recall it with mcp__dmem__recall; persist new durable user facts with the dmem tools (remember / log_*) - do not build a profile in this file.";

/// Config homes for the CodeBuddy agent runtime. Tencent's WorkBuddy app embeds it with
/// `CODEBUDDY_CONFIG_DIR` pointed at its app home - `~/.workbuddy-ai` on current builds,
/// `~/.workbuddy` on earlier ones (a 2026-07 auto-update renamed it, leaving the old dir
/// behind; verified via the app's sandbox `--app_home` and which dir holds live settings) -
/// while a standalone CodeBuddy CLI/IDE install uses the default `~/.codebuddy`. All speak
/// the Claude Code protocol (settings.json hooks with hookSpecificOutput.additionalContext,
/// user-scope `.mcp.json`), so one flag wires every home that exists.
fn workbuddy_homes() -> Result<Vec<(&'static str, PathBuf)>> {
    let h = home()?;
    Ok([
        ("WorkBuddy", h.join(".workbuddy-ai")),
        ("WorkBuddy (pre-rename home)", h.join(".workbuddy")),
        ("CodeBuddy CLI", h.join(".codebuddy")),
    ]
    .into_iter()
    .filter(|(_, d)| d.exists())
    .collect())
}

/// WorkBuddy settings.json: dm hooks + native auto-memory OFF (+ for app homes, enabling the
/// dmem MCP plugin), in ONE read-modify-write (so `.dmbak` holds the true pre-run state).
/// autoMemoryEnabled=false while dmem is wired - the built-in MEMORY.md auto-memory/extraction
/// would build a second, drifting store next to dmem (it also gates memoryExtraction, which
/// requires it). On remove the keys are dropped, returning WorkBuddy to its stock defaults.
/// Returns false when the file is not strict JSON.
fn workbuddy_settings_install(path: &Path, dm: &str, enable_plugin: bool, remove: bool) -> Result<bool> {
    let mut root: Value = if path.exists() {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
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
    if path.exists() {
        let _ = std::fs::copy(path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    merge_dm_hooks(&mut root, dm, remove);
    let obj = root.as_object_mut().unwrap();
    if remove {
        if let Some(mem) = obj.get_mut("memory").and_then(|m| m.as_object_mut()) {
            mem.remove("autoMemoryEnabled");
        }
        if obj.get("memory").and_then(|m| m.as_object()).map(|m| m.is_empty()).unwrap_or(false) {
            obj.remove("memory");
        }
    } else {
        let mem = obj.entry("memory").or_insert_with(|| json!({}));
        if !mem.is_object() {
            *mem = json!({});
        }
        mem.as_object_mut().unwrap().insert("autoMemoryEnabled".to_string(), json!(false));
    }
    // App homes load MCP servers through PLUGINS, not the user .mcp.json (sessions are created
    // with strictMcpConfig), so the dmem plugin written next door must be switched on here.
    if remove || !enable_plugin {
        if let Some(ep) = obj.get_mut("enabledPlugins").and_then(|m| m.as_object_mut()) {
            ep.remove("dmem@dmem");
        }
        if obj.get("enabledPlugins").and_then(|m| m.as_object()).map(|m| m.is_empty()).unwrap_or(false) {
            obj.remove("enabledPlugins");
        }
    } else {
        let ep = obj.entry("enabledPlugins").or_insert_with(|| json!({}));
        if !ep.is_object() {
            *ep = json!({});
        }
        ep.as_object_mut().unwrap().insert("dmem@dmem".to_string(), json!(true));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// User-scope MCP file inside a CodeBuddy config home. The runtime resolves the first existing
/// of `.mcp.json` / `mcp.json` there (falling back to the OS-home `~/.mcp.json`), defaulting to
/// `.mcp.json` when none exist - so edit the file it would pick and create the dot-file default.
/// Root shape is the standard `{"mcpServers": {...}}` map, hence `desktop_merge`. Returns the
/// edited path, or None when the existing file is not strict JSON (refused, untouched).
fn workbuddy_mcp_install(dir: &Path, dm: &str, remove: bool) -> Result<Option<PathBuf>> {
    let path = [dir.join(".mcp.json"), dir.join("mcp.json")]
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| dir.join(".mcp.json"));
    let existing: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        }
    } else {
        json!({})
    };
    if path.exists() {
        let _ = std::fs::copy(&path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    let root = desktop_merge(existing, dm, remove);
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

/// Write the dmem local marketplace + plugin for a WorkBuddy app home. The app creates agent
/// sessions with strictMcpConfig, which drops the user-scope `.mcp.json` servers - but servers
/// declared by an ENABLED PLUGIN still connect (scope=plugin), so the app path to the dmem save
/// tools is a plugin whose manifest carries `mcpServers` inline. Layout mirrors the app's own
/// local marketplaces (e.g. the session-created `experts` one): a directory marketplace with
/// `.codebuddy-plugin/marketplace.json` and `plugins/dmem/.codebuddy-plugin/plugin.json`.
fn workbuddy_write_plugin(mp_dir: &Path, dm: &str) -> Result<()> {
    let plug = mp_dir.join("plugins/dmem");
    std::fs::create_dir_all(mp_dir.join(".codebuddy-plugin"))?;
    std::fs::create_dir_all(plug.join(".codebuddy-plugin"))?;
    let market = json!({ "name": "dmem", "plugins": [ { "name": "dmem", "source": "./plugins/dmem" } ] });
    std::fs::write(mp_dir.join(".codebuddy-plugin/marketplace.json"), serde_json::to_string_pretty(&market)? + "\n")?;
    let manifest = json!({
        "name": "dmem",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Shared cross-tool memory save tools, backed by dm-lite (dmem): recall, remember, log_decision/lesson/incident, add_reminder, forget. Persona + per-prompt recall ride the settings.json hooks; this plugin carries the MCP server.",
        "license": "MIT",
        "mcpServers": { "dmem": { "command": dm, "args": ["mcp"] } }
    });
    std::fs::write(plug.join(".codebuddy-plugin/plugin.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    Ok(())
}

/// Register (or with `remove`, drop) the dmem marketplace in the app home's
/// `plugins/known_marketplaces.json` - the same directory-type record the app writes for its
/// own local marketplaces. Read-modify-write with the usual strict-JSON refusal + `.dmbak`
/// backup; every other marketplace entry is preserved. Returns false when refused.
fn workbuddy_register_marketplace(home_dir: &Path, mp_dir: &Path, remove: bool) -> Result<bool> {
    let path = home_dir.join("plugins/known_marketplaces.json");
    let mut root: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
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
    if path.exists() {
        let _ = std::fs::copy(&path, PathBuf::from(format!("{}.dmbak", path.display())));
    }
    let obj = root.as_object_mut().unwrap();
    obj.remove("dmem");
    if !remove {
        let mp = mp_dir.to_string_lossy();
        obj.insert(
            "dmem".to_string(),
            json!({
                "type": "directory",
                "source": { "source": "directory", "path": mp },
                "installLocation": mp,
                "description": "dm-lite (dmem) shared memory - local marketplace written by `dmem bootstrap --workbuddy`",
                "autoUpdate": false,
                "lastUpdated": rfc3339_utc()
            }),
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = serde_json::to_string_pretty(&root)?;
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Project the dmem persona into WorkBuddy's identity files. Only the WorkBuddy app layer injects
/// SOUL.md/IDENTITY.md/USER.md into context (every turn); a plain CodeBuddy CLI home has no such
/// files, so touch only what exists - the CLI still gets persona via the SessionStart hook.
/// SOUL.md gets the full persona/protocol block; IDENTITY.md and USER.md (whose stock content
/// invites the agent to grow them) get a short source-of-truth pointer. User content outside the
/// markers is preserved.
fn workbuddy_sync_persona(dir: &Path, remove: bool) -> Result<String> {
    let soul = dir.join("SOUL.md");
    if !soul.exists() {
        return Ok("no SOUL.md in this home (plain CLI) - persona rides the SessionStart hook".to_string());
    }
    let block = if remove { None } else { Some(persona_block(WB_SOUL_LEAD)?) };
    upsert_managed_block(&soul, WB_BEGIN, SOUL_END, block.as_deref())?;
    for (file, note) in [("IDENTITY.md", WB_IDENTITY_NOTE), ("USER.md", WB_USER_NOTE)] {
        let p = dir.join(file);
        if p.exists() {
            upsert_managed_block(&p, WB_BEGIN, SOUL_END, if remove { None } else { Some(note) })?;
        }
    }
    Ok(if remove {
        "removed the dmem-managed blocks from SOUL.md / IDENTITY.md / USER.md".to_string()
    } else {
        "persona + protocols -> SOUL.md (injected every turn); IDENTITY.md/USER.md now point at dmem as source of truth".to_string()
    })
}

/// WorkBuddy / CodeBuddy: wire dmem hooks + native-auto-memory-off into settings.json, the MCP
/// save tools into the user-scope `.mcp.json`, and project persona/protocols into the identity
/// files (WorkBuddy homes only). Applied to every CodeBuddy config home present.
fn workbuddy_install(dm: &str, remove: bool) -> Result<()> {
    let homes = workbuddy_homes()?;
    if homes.is_empty() {
        println!("  skip WorkBuddy/CodeBuddy (no ~/.workbuddy or ~/.codebuddy)");
        return Ok(());
    }
    for (name, dir) in homes {
        // An app home injects the identity files and creates sessions with strictMcpConfig,
        // so it additionally needs the dmem MCP plugin; a plain CLI home reads `.mcp.json`.
        let is_app = dir.file_name().map(|f| f.to_string_lossy().starts_with(".workbuddy")).unwrap_or(false);
        let mp_dir = dir.join("plugins/marketplaces/dmem");
        let plugin_ok = if is_app {
            if !remove {
                workbuddy_write_plugin(&mp_dir, dm)?;
            }
            let registered = workbuddy_register_marketplace(&dir, &mp_dir, remove)?;
            if remove {
                let _ = std::fs::remove_dir_all(&mp_dir);
            }
            registered
        } else {
            false
        };
        let settings = dir.join("settings.json");
        if !workbuddy_settings_install(&settings, dm, is_app && plugin_ok && !remove, remove)? {
            println!("  skip {name} -> {} is not valid JSON; fix it and re-run (nothing was changed).", settings.display());
            continue;
        }
        let mcp = workbuddy_mcp_install(&dir, dm, remove)?;
        let persona_status = match workbuddy_sync_persona(&dir, remove) {
            Ok(s) => s,
            Err(e) => format!("persona projection skipped ({e:#})"),
        };
        if remove {
            println!("  unwired {name} -> {} (hooks removed; native auto-memory back to its stock default)", settings.display());
            match mcp {
                Some(p) => println!("    removed mcpServers.dmem from {}", p.display()),
                None => println!("    MCP file is not strict JSON - remove its mcpServers.dmem entry manually."),
            }
            if is_app {
                println!("    removed the dmem plugin marketplace + its known_marketplaces.json entry");
            }
            println!("    {persona_status}");
        } else {
            println!("  wired {name} -> {} (hooks: persona on SessionStart, recall + save nudge per prompt; native auto-memory OFF)", settings.display());
            match mcp {
                Some(p) => println!("    MCP save tools -> {}", p.display()),
                None => println!("    MCP file is not strict JSON - add {{\"mcpServers\":{{\"dmem\":{{\"command\":{},\"args\":[\"mcp\"]}}}}}} manually.", serde_json::to_string(dm)?),
            }
            if is_app {
                if plugin_ok {
                    println!("    dmem MCP plugin -> {} (enabled as dmem@dmem; app sessions use strictMcpConfig and load MCP via plugins, not .mcp.json)", mp_dir.display());
                } else {
                    println!("    WARN: plugins/known_marketplaces.json is not strict JSON - app sessions will NOT see the dmem tools until the dmem marketplace is registered there manually.");
                }
            }
            println!("    {persona_status}");
            println!("    restart {name} so it reloads settings, hooks, plugins and MCP servers.");
        }
    }
    Ok(())
}

pub fn run(devin: bool, claude: bool, codex: bool, hermes: bool, opencode: bool, grok: bool, workbuddy: bool, claude_desktop: bool, cowork: bool) -> Result<()> {
    run_mode(devin, claude, codex, hermes, opencode, grok, workbuddy, claude_desktop, cowork, false)
}

/// Wire or (with `remove`) unwire dmem into the selected agents. Devin + Claude Code use the
/// generic Claude-compatible settings.json hook merge; Codex uses a bespoke `~/.codex/config.toml`
/// MCP+plugin installer; Hermes uses a `~/.hermes/config.yaml` MCP+shell-hook installer; OpenCode
/// gets a TypeScript plugin + `mcp.dmem` entry in ~/.config/opencode; Grok gets an MCP entry via
/// `grok mcp add` (its v0 hooks cannot inject context, so no hook plugin); WorkBuddy/CodeBuddy
/// gets CC-shape hooks + `.mcp.json` + persona projection with native auto-memory disabled;
/// Claude Desktop (hook-less) gets an MCP entry in claude_desktop_config.json; Claude Cowork
/// (macOS) gets that same MCP entry plus a generated hook plugin (.plugin zip) for manual upload
/// in the app.
pub fn run_mode(devin: bool, claude: bool, codex: bool, hermes: bool, opencode: bool, grok: bool, workbuddy: bool, claude_desktop: bool, cowork: bool, remove: bool) -> Result<()> {
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

    if workbuddy {
        workbuddy_install(&dm, remove)?;
        did_any = true;
    }

    if claude_desktop {
        claude_desktop_install(&dm, remove)?;
        did_any = true;
    }

    if cowork {
        cowork_install(&dm, remove, claude_desktop)?;
        did_any = true;
    }

    if !did_any {
        println!("Nothing changed. Pass --devin / --claude / --codex / --hermes / --opencode / --grok / --workbuddy / --claude-desktop / --cowork (or --all), and ensure the agent is installed.");
        return Ok(());
    }
    println!();
    if remove {
        println!("Done. dmem hooks removed (the agent's other hooks/plugins are untouched).");
    } else {
        println!("Done. dmem is wired in (SessionStart -> persona/recent, UserPromptSubmit -> recall + save nudge).");
        println!("Undo any time with:  dmem bootstrap --remove --devin / --claude / --codex / --hermes / --opencode / --grok / --workbuddy / --claude-desktop / --cowork");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sh_quote_passes_safe_paths_and_quotes_hostile_ones() {
        assert_eq!(sh_quote("/home/user/.local/bin/dmem"), "/home/user/.local/bin/dmem", "safe path unchanged (existing configs stay byte-identical)");
        assert_eq!(sh_quote("/Users/Jane Doe/bin/dmem"), "'/Users/Jane Doe/bin/dmem'", "space -> quoted");
        assert_eq!(sh_quote("/tmp/x;rm -rf ~/dmem"), "'/tmp/x;rm -rf ~/dmem'", "metacharacters -> quoted");
        assert_eq!(sh_quote("/tmp/it's/dmem"), r"'/tmp/it'\''s/dmem'", "embedded single quote escaped");
    }

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
            "plugin": ["file:///home/u/.config/opencode/plugins/status-line.js", "example-auth-plugin@latest"],
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
    fn workbuddy_settings_wires_hooks_and_disables_auto_memory() {
        let dir = std::env::temp_dir().join(format!("dmwb-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("settings.json");
        // pre-existing user state: an unrelated hook, an unrelated memory subkey, a sibling key
        std::fs::write(
            &cfg,
            r#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"/other/tool x"}]}]},"memory":{"memoryExtraction":true},"theme":"dark"}"#,
        )
        .unwrap();

        assert!(workbuddy_settings_install(&cfg, "/path/to/dmem", true, false).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().contains("hook user_prompt_submit"));
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2, "unrelated hook preserved");
        assert_eq!(v["memory"]["autoMemoryEnabled"], false, "native auto-memory disabled");
        assert_eq!(v["memory"]["memoryExtraction"], true, "unrelated memory subkey preserved");
        assert_eq!(v["enabledPlugins"]["dmem@dmem"], true, "MCP plugin enabled");
        assert_eq!(v["theme"], "dark");
        assert!(dir.join("settings.json.dmbak").exists(), "backup written");

        // idempotent re-run: still one dm entry per event, still disabled
        assert!(workbuddy_settings_install(&cfg, "/path/to/dmem", true, false).unwrap());
        let v2: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v2["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(v2["memory"]["autoMemoryEnabled"], false);

        // remove: our hooks + plugin enable gone, autoMemoryEnabled dropped (stock default),
        // the rest intact
        assert!(workbuddy_settings_install(&cfg, "/path/to/dmem", true, true).unwrap());
        let v3: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v3["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(v3["hooks"].get("UserPromptSubmit").is_none());
        assert!(v3["memory"].get("autoMemoryEnabled").is_none(), "key dropped -> stock default");
        assert_eq!(v3["memory"]["memoryExtraction"], true);
        assert!(v3.get("enabledPlugins").is_none(), "our enable dropped, empty table removed");

        // a CLI home (enable_plugin=false) never grows an enabledPlugins entry
        let cli = dir.join("cli.json");
        assert!(workbuddy_settings_install(&cli, "/path/to/dmem", false, false).unwrap());
        let vc: Value = serde_json::from_str(&std::fs::read_to_string(&cli).unwrap()).unwrap();
        assert!(vc.get("enabledPlugins").is_none());

        // a user's OTHER enabled plugin survives install + remove
        let other = dir.join("other.json");
        std::fs::write(&other, r#"{"enabledPlugins":{"weixinpay@workbuddy-builtin":true}}"#).unwrap();
        assert!(workbuddy_settings_install(&other, "/path/to/dmem", true, false).unwrap());
        assert!(workbuddy_settings_install(&other, "/path/to/dmem", true, true).unwrap());
        let vo: Value = serde_json::from_str(&std::fs::read_to_string(&other).unwrap()).unwrap();
        assert_eq!(vo["enabledPlugins"]["weixinpay@workbuddy-builtin"], true);
        assert!(vo["enabledPlugins"].get("dmem@dmem").is_none());

        // a memory table we created ourselves is dropped entirely on remove
        let fresh = dir.join("fresh.json");
        assert!(workbuddy_settings_install(&fresh, "/path/to/dmem", false, false).unwrap());
        assert!(workbuddy_settings_install(&fresh, "/path/to/dmem", false, true).unwrap());
        let v4: Value = serde_json::from_str(&std::fs::read_to_string(&fresh).unwrap()).unwrap();
        assert!(v4.get("memory").is_none(), "empty memory table removed");

        // not-strict-JSON settings are refused untouched
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "{ oops").unwrap();
        assert!(!workbuddy_settings_install(&broken, "/path/to/dmem", true, false).unwrap());
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{ oops");
    }

    #[test]
    fn workbuddy_plugin_and_marketplace_roundtrip() {
        let home = std::env::temp_dir().join(format!("dmwbp-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&home).unwrap();
        let mp = home.join("plugins/marketplaces/dmem");
        workbuddy_write_plugin(&mp, "/abs/dmem").unwrap();
        let m: Value = serde_json::from_str(&std::fs::read_to_string(mp.join(".codebuddy-plugin/marketplace.json")).unwrap()).unwrap();
        assert_eq!(m["plugins"][0]["source"], "./plugins/dmem");
        let p: Value = serde_json::from_str(&std::fs::read_to_string(mp.join("plugins/dmem/.codebuddy-plugin/plugin.json")).unwrap()).unwrap();
        assert_eq!(p["mcpServers"]["dmem"]["command"], "/abs/dmem");
        assert_eq!(p["mcpServers"]["dmem"]["args"][0], "mcp");

        // registration preserves the app's other marketplaces and is idempotent
        let known = home.join("plugins/known_marketplaces.json");
        std::fs::write(&known, r#"{"workbuddy-builtin":{"type":"directory","autoUpdate":false}}"#).unwrap();
        assert!(workbuddy_register_marketplace(&home, &mp, false).unwrap());
        assert!(workbuddy_register_marketplace(&home, &mp, false).unwrap());
        let k: Value = serde_json::from_str(&std::fs::read_to_string(&known).unwrap()).unwrap();
        assert_eq!(k["dmem"]["type"], "directory");
        assert_eq!(k["dmem"]["installLocation"], mp.to_string_lossy().as_ref());
        assert_eq!(k["workbuddy-builtin"]["type"], "directory", "other marketplace preserved");
        // remove drops only ours
        assert!(workbuddy_register_marketplace(&home, &mp, true).unwrap());
        let k2: Value = serde_json::from_str(&std::fs::read_to_string(&known).unwrap()).unwrap();
        assert!(k2.get("dmem").is_none());
        assert!(k2.get("workbuddy-builtin").is_some());
        // a broken registry is refused untouched
        std::fs::write(&known, "{ nope").unwrap();
        assert!(!workbuddy_register_marketplace(&home, &mp, false).unwrap());
        assert_eq!(std::fs::read_to_string(&known).unwrap(), "{ nope");
    }

    #[test]
    fn workbuddy_mcp_prefers_existing_file_and_defaults_to_dotfile() {
        let dir = std::env::temp_dir().join(format!("dmwbm-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        // no file yet -> the runtime's default (.mcp.json) is created
        let p = workbuddy_mcp_install(&dir, "/abs/dmem", false).unwrap().unwrap();
        assert_eq!(p, dir.join(".mcp.json"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["dmem"]["command"], "/abs/dmem");
        std::fs::remove_file(&p).unwrap();
        // an existing bare mcp.json (with another server) is the resolver's pick -> edited in place
        let bare = dir.join("mcp.json");
        std::fs::write(&bare, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let p = workbuddy_mcp_install(&dir, "/abs/dmem", false).unwrap().unwrap();
        assert_eq!(p, bare);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&bare).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["dmem"]["command"], "/abs/dmem");
        assert_eq!(v["mcpServers"]["other"]["command"], "x", "unrelated server preserved");
        assert!(!dir.join(".mcp.json").exists(), "no second MCP file created");
        // remove drops only our entry
        workbuddy_mcp_install(&dir, "/abs/dmem", true).unwrap().unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&bare).unwrap()).unwrap();
        assert!(v["mcpServers"].get("dmem").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        // a JSONC/broken file is refused untouched
        std::fs::write(&bare, "// hi\n{}").unwrap();
        assert!(workbuddy_mcp_install(&dir, "/abs/dmem", false).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&bare).unwrap(), "// hi\n{}");
    }

    #[test]
    fn upsert_managed_block_inserts_replaces_and_removes_preserving_user_content() {
        let dir = std::env::temp_dir().join(format!("dmblk-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("SOUL.md");
        std::fs::write(&f, "# My Soul\n\nUser prose stays.\n").unwrap();
        upsert_managed_block(&f, WB_BEGIN, SOUL_END, Some("BLOCK v1")).unwrap();
        let s = std::fs::read_to_string(&f).unwrap();
        assert!(s.starts_with("# My Soul"), "user content first, verbatim");
        assert!(s.contains("BLOCK v1"));
        // replace, not append: re-sync swaps the block content, one fence only
        upsert_managed_block(&f, WB_BEGIN, SOUL_END, Some("BLOCK v2")).unwrap();
        let s = std::fs::read_to_string(&f).unwrap();
        assert!(!s.contains("BLOCK v1"));
        assert!(s.contains("BLOCK v2"));
        assert_eq!(s.matches(WB_BEGIN).count(), 1);
        assert!(s.contains("User prose stays."));
        // remove: only the fenced block goes
        upsert_managed_block(&f, WB_BEGIN, SOUL_END, None).unwrap();
        let s = std::fs::read_to_string(&f).unwrap();
        assert!(!s.contains("BLOCK v2"));
        assert!(s.contains("User prose stays."));
        // a file that was ONLY our block becomes empty, not deleted
        let solo = dir.join("solo.md");
        upsert_managed_block(&solo, WB_BEGIN, SOUL_END, Some("X")).unwrap();
        upsert_managed_block(&solo, WB_BEGIN, SOUL_END, None).unwrap();
        assert!(solo.exists());
        assert_eq!(std::fs::read_to_string(&solo).unwrap(), "");
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

    #[test]
    fn hermes_toggle_plugin_enabled_replaces_v1_daimon() {
        use serde_yaml_ng::{Mapping, Value as Y};
        let mut root = Mapping::new();
        let mut plugins = Mapping::new();
        plugins.insert(
            Y::from("enabled"),
            Y::Sequence(vec![Y::from("daimon"), Y::from("security-guidance")]),
        );
        root.insert(Y::from("plugins"), Y::Mapping(plugins));

        hermes_toggle_plugin_enabled(&mut root, false);
        let enabled: Vec<&str> = root
            .get("plugins")
            .unwrap()
            .as_mapping()
            .unwrap()
            .get("enabled")
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(enabled, vec!["security-guidance", "dmem"]);

        hermes_toggle_plugin_enabled(&mut root, false);
        let enabled2: Vec<&str> = root
            .get("plugins")
            .unwrap()
            .as_mapping()
            .unwrap()
            .get("enabled")
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(enabled2, vec!["security-guidance", "dmem"], "idempotent");

        hermes_toggle_plugin_enabled(&mut root, true);
        let enabled3: Vec<&str> = root
            .get("plugins")
            .unwrap()
            .as_mapping()
            .unwrap()
            .get("enabled")
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(enabled3, vec!["security-guidance"]);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dmboot-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn codex_plugin_hooks_carry_additional_context_limits() {
        let mp = scratch("codex");
        codex_write_plugin(&mp, "/opt/dmem").unwrap();
        let hooks = read_json(&mp.join("plugins/dmem/hooks/hooks.json"));
        let ss = &hooks["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(ss["type"], "command");
        assert_eq!(ss["command"], "/opt/dmem hook session_start");
        assert_eq!(ss["timeout"], 10);
        assert_eq!(ss["additionalContextLimit"], CODEX_SESSION_START_CONTEXT_LIMIT);
        let ups = &hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(ups["type"], "command");
        assert_eq!(ups["command"], "/opt/dmem hook user_prompt_submit");
        assert_eq!(ups["timeout"], 8);
        assert_eq!(ups["additionalContextLimit"], CODEX_USER_PROMPT_SUBMIT_CONTEXT_LIMIT);
        // The limit exists to keep the whole session-start block in context, so it must clear
        // Codex's 2,500-token default with room for persona + protocol growth.
        assert!(CODEX_SESSION_START_CONTEXT_LIMIT > 2_500);
        assert!(CODEX_USER_PROMPT_SUBMIT_CONTEXT_LIMIT > 2_500);
        let _ = std::fs::remove_dir_all(&mp);
    }

    #[test]
    fn codex_plugin_tree_is_self_consistent() {
        let mp = scratch("codex-tree");
        codex_write_plugin(&mp, "/opt/dmem").unwrap();
        let market = read_json(&mp.join(".claude-plugin/marketplace.json"));
        assert_eq!(market["name"], "dmem");
        assert_eq!(market["plugins"][0]["source"], "./plugins/dmem");
        let plug = mp.join("plugins/dmem");
        let manifest = read_json(&plug.join(".codex-plugin/plugin.json"));
        assert_eq!(manifest["name"], "dmem");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        let hooks_rel = manifest["hooks"].as_str().unwrap();
        assert!(plug.join(hooks_rel).is_file(), "manifest hooks path must resolve inside the plugin");
        let _ = std::fs::remove_dir_all(&mp);
    }

    #[test]
    fn codex_hooks_shell_quote_the_binary_path() {
        let hooks = codex_hooks("/Users/some one/.local/bin/dmem");
        let cmd = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/Users/some one/.local/bin/dmem' hook session_start");
    }

    #[test]
    fn cowork_hooks_match_claude_code_shape() {
        let hooks = cowork_hooks("/opt/dmem");
        let ss = &hooks["hooks"]["SessionStart"][0];
        // Cowork resumes per turn; skip `resume` (output not attached), keep startup/clear/compact
        assert_eq!(ss["matcher"], COWORK_SESSION_START_MATCHER);
        for src in ["startup", "clear", "compact"] {
            assert!(COWORK_SESSION_START_MATCHER.split('|').any(|m| m == src), "{src} must be matched");
        }
        assert!(!COWORK_SESSION_START_MATCHER.split('|').any(|m| m == "resume"));
        assert_eq!(ss["hooks"][0]["type"], "command");
        assert_eq!(ss["hooks"][0]["command"], "/opt/dmem hook session_start");
        assert_eq!(ss["hooks"][0]["timeout"], 10);
        let ups = &hooks["hooks"]["UserPromptSubmit"][0];
        assert_eq!(ups["matcher"], "");
        assert_eq!(ups["hooks"][0]["type"], "command");
        assert_eq!(ups["hooks"][0]["command"], "/opt/dmem hook user_prompt_submit");
        assert_eq!(ups["hooks"][0]["timeout"], 8);
        // Claude Code forbids context injection on SessionEnd, and additionalContextLimit is a
        // Codex-only key: neither belongs in a plugin the Claude Code engine loads.
        assert!(hooks["hooks"].get("SessionEnd").is_none());
        assert!(ss["hooks"][0].get("additionalContextLimit").is_none());
        assert_eq!(hooks["hooks"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn cowork_hooks_shell_quote_the_binary_path() {
        let hooks = cowork_hooks("/Users/some one/.local/bin/dmem");
        let cmd = hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/Users/some one/.local/bin/dmem' hook user_prompt_submit");
    }

    #[test]
    fn cowork_manifest_is_kebab_and_tracks_crate_version() {
        let m = cowork_manifest();
        assert_eq!(m["name"], COWORK_PLUGIN_NAME);
        assert!(COWORK_PLUGIN_NAME.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert!(m["author"]["name"].is_string());
        // default hooks/ discovery, and no bundled MCP server (would duplicate the desktop entry)
        assert!(m.get("hooks").is_none());
        assert!(m.get("mcpServers").is_none());
    }

    #[test]
    fn cowork_plugin_tree_is_self_consistent() {
        let root = scratch("cowork-tree");
        let zip = cowork_write_plugin(&root, "/opt/dmem").unwrap();
        let plug = root.join(COWORK_PLUGIN_NAME);
        let manifest = read_json(&plug.join(".claude-plugin/plugin.json"));
        assert_eq!(manifest["name"], COWORK_PLUGIN_NAME);
        let hooks = read_json(&plug.join("hooks/hooks.json"));
        assert_eq!(hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"], "/opt/dmem hook session_start");
        assert!(!plug.join(".mcp.json").exists(), "plugin must not bundle a second dmem MCP server");
        assert_eq!(zip, root.join("dmem-cowork.plugin"));
        assert!(zip.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cowork_write_is_idempotent_deterministic_and_clears_stale_files() {
        let root = scratch("cowork-idem");
        let zip = cowork_write_plugin(&root, "/opt/dmem").unwrap();
        let first = std::fs::read(&zip).unwrap();
        let stray = root.join(COWORK_PLUGIN_NAME).join("hooks").join("old.json");
        std::fs::write(&stray, "{}").unwrap();
        cowork_write_plugin(&root, "/opt/dmem").unwrap();
        let second = std::fs::read(&zip).unwrap();
        assert_eq!(first, second, "re-run must produce byte-identical zip");
        assert!(!stray.exists(), "tree is rebuilt from scratch; stray files do not survive");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cowork_remove_deletes_dir_and_zip() {
        let root = scratch("cowork-rm");
        assert!(!cowork_remove(&root).unwrap(), "nothing to remove yet");
        let zip = cowork_write_plugin(&root, "/opt/dmem").unwrap();
        assert!(cowork_remove(&root).unwrap());
        assert!(!zip.exists());
        assert!(!root.join(COWORK_PLUGIN_NAME).exists());
        assert!(root.exists(), "the hand-over dir itself is left in place");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crc32_matches_the_ieee_check_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    fn le16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn le32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn stored_zip_structure_is_valid() {
        let root = scratch("zip");
        let out = root.join("t.zip");
        let a: &[u8] = b"{\"a\":1}\n";
        let b: &[u8] = b"hooks!";
        write_stored_zip(&[("x/a.json", a), ("b.txt", b)], &out).unwrap();
        let z = std::fs::read(&out).unwrap();
        // EOCD is the last 22 bytes (no comment)
        let eocd = z.len() - 22;
        assert_eq!(le32(&z, eocd), 0x0605_4b50);
        assert_eq!(le16(&z, eocd + 8), 2, "entries on this disk");
        assert_eq!(le16(&z, eocd + 10), 2, "entries total");
        let cd_size = le32(&z, eocd + 12) as usize;
        let cd_off = le32(&z, eocd + 16) as usize;
        assert_eq!(cd_off + cd_size, eocd, "central dir runs right up to the EOCD");
        // first local header at 0, stored, crc + sizes + name
        assert_eq!(le32(&z, 0), 0x0403_4b50);
        assert_eq!(le16(&z, 8), 0, "method stored");
        assert_eq!(le32(&z, 14), crc32(a));
        assert_eq!(le32(&z, 18) as usize, a.len());
        assert_eq!(le32(&z, 22) as usize, a.len());
        let n1 = le16(&z, 26) as usize;
        assert_eq!(&z[30..30 + n1], b"x/a.json");
        assert_eq!(&z[30 + n1..30 + n1 + a.len()], a);
        // second local header follows immediately
        let off2 = 30 + n1 + a.len();
        assert_eq!(le32(&z, off2), 0x0403_4b50);
        assert_eq!(le32(&z, off2 + 14), crc32(b));
        // central directory: two headers whose offsets point at the local headers
        assert_eq!(le32(&z, cd_off), 0x0201_4b50);
        assert_eq!(le32(&z, cd_off + 42), 0, "first entry offset");
        let cn1 = le16(&z, cd_off + 28) as usize;
        assert_eq!(&z[cd_off + 46..cd_off + 46 + cn1], b"x/a.json");
        let cd2 = cd_off + 46 + cn1;
        assert_eq!(le32(&z, cd2), 0x0201_4b50);
        assert_eq!(le32(&z, cd2 + 42) as usize, off2, "second entry offset");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cross-check against the system unzip where one exists (macOS/Linux dev boxes); skipped
    /// silently elsewhere so CI runners without it still pass.
    #[test]
    fn stored_zip_passes_system_unzip_when_available() {
        if std::process::Command::new("unzip").arg("-v").output().is_err() {
            return;
        }
        let root = scratch("cowork-unzip");
        let zip = cowork_write_plugin(&root, "/opt/dmem").unwrap();
        let out = std::process::Command::new("unzip").args(["-t", zip.to_str().unwrap()]).output().unwrap();
        assert!(out.status.success(), "unzip -t failed: {}", String::from_utf8_lossy(&out.stdout));
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(listing.contains(".claude-plugin/plugin.json"));
        assert!(listing.contains("hooks/hooks.json"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cowork_registry_merge_adds_user_scoped_entry_keeps_others_and_installed_at() {
        let existing = json!({"version": 2, "plugins": {"other@mkt": [{"scope": "user", "installPath": "/x"}]}});
        let v = cowork_registry_merge(existing, "/ws/cowork_plugins/dmem-cowork", false, "2026-09-09T00:00:00Z");
        assert_eq!(v["version"], 2);
        assert_eq!(v["plugins"]["other@mkt"][0]["installPath"], "/x", "other plugins preserved");
        let e = &v["plugins"][COWORK_PLUGIN_ID][0];
        assert_eq!(e["scope"], "user", "Cowork loads non-project plugins only with scope user");
        assert_eq!(e["installPath"], "/ws/cowork_plugins/dmem-cowork");
        assert_eq!(e["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(e["installedAt"], "2026-09-09T00:00:00Z");
        assert_eq!(e["lastUpdated"], "2026-09-09T00:00:00Z");
        // re-run later: installedAt sticks, lastUpdated moves, still exactly one entry
        let v2 = cowork_registry_merge(v, "/ws/cowork_plugins/dmem-cowork", false, "2026-09-10T00:00:00Z");
        assert_eq!(v2["plugins"][COWORK_PLUGIN_ID].as_array().unwrap().len(), 1);
        assert_eq!(v2["plugins"][COWORK_PLUGIN_ID][0]["installedAt"], "2026-09-09T00:00:00Z");
        assert_eq!(v2["plugins"][COWORK_PLUGIN_ID][0]["lastUpdated"], "2026-09-10T00:00:00Z");
        // remove drops only ours
        let v3 = cowork_registry_merge(v2, "", true, "2026-09-11T00:00:00Z");
        assert!(v3["plugins"].get(COWORK_PLUGIN_ID).is_none());
        assert!(v3["plugins"].get("other@mkt").is_some());
        // malformed input is replaced defensively
        let v4 = cowork_registry_merge(json!("garbage"), "/p", false, "t");
        assert_eq!(v4["version"], 2);
        assert!(v4["plugins"][COWORK_PLUGIN_ID].is_array());
    }

    #[test]
    fn cowork_settings_merge_toggles_only_our_flag() {
        let existing = json!({"someAppKey": {"a": 1}, "enabledPlugins": {"other@mkt": true}});
        let v = cowork_settings_merge(existing, false);
        assert_eq!(v["someAppKey"]["a"], 1);
        assert_eq!(v["enabledPlugins"]["other@mkt"], true);
        assert_eq!(v["enabledPlugins"][COWORK_PLUGIN_ID], true);
        let v2 = cowork_settings_merge(v, true);
        assert!(v2["enabledPlugins"].get(COWORK_PLUGIN_ID).is_none());
        assert_eq!(v2["enabledPlugins"]["other@mkt"], true);
        assert_eq!(v2["someAppKey"]["a"], 1);
        assert_eq!(cowork_settings_merge(json!(null), false)["enabledPlugins"][COWORK_PLUGIN_ID], true);
    }

    #[test]
    fn cowork_local_install_writes_tree_registry_and_settings_then_removes_cleanly() {
        let ws = scratch("cowork-ws");
        std::fs::write(ws.join("cowork_settings.json"), r#"{"keep":true}"#).unwrap();
        assert!(cowork_local_install(&ws, "/opt/dmem", false).unwrap());
        let plug = ws.join("cowork_plugins").join(COWORK_PLUGIN_NAME);
        assert!(plug.join(".claude-plugin/plugin.json").is_file());
        let hooks = read_json(&plug.join("hooks/hooks.json"));
        assert_eq!(hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "/opt/dmem hook user_prompt_submit");
        let reg = read_json(&ws.join("cowork_plugins/installed_plugins.json"));
        let e = &reg["plugins"][COWORK_PLUGIN_ID][0];
        assert_eq!(e["installPath"], plug.to_string_lossy().as_ref(), "install path is inside cowork_plugins/");
        assert!(e["installPath"].as_str().unwrap().starts_with(ws.join("cowork_plugins").to_string_lossy().as_ref()));
        let set = read_json(&ws.join("cowork_settings.json"));
        assert_eq!(set["keep"], true, "app's own settings keys preserved");
        assert_eq!(set["enabledPlugins"][COWORK_PLUGIN_ID], true);
        assert!(ws.join("cowork_settings.json.dmbak").is_file(), "existing app file backed up");
        // idempotent
        assert!(cowork_local_install(&ws, "/opt/dmem", false).unwrap());
        assert_eq!(read_json(&ws.join("cowork_plugins/installed_plugins.json"))["plugins"][COWORK_PLUGIN_ID].as_array().unwrap().len(), 1);
        // remove
        assert!(cowork_local_install(&ws, "/opt/dmem", true).unwrap());
        assert!(!plug.exists());
        assert!(read_json(&ws.join("cowork_plugins/installed_plugins.json"))["plugins"].get(COWORK_PLUGIN_ID).is_none());
        assert!(read_json(&ws.join("cowork_settings.json"))["enabledPlugins"].get(COWORK_PLUGIN_ID).is_none());
        assert_eq!(read_json(&ws.join("cowork_settings.json"))["keep"], true);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cowork_local_install_refuses_unparseable_app_files() {
        let ws = scratch("cowork-badjson");
        std::fs::write(ws.join("cowork_settings.json"), "{not json").unwrap();
        assert!(!cowork_local_install(&ws, "/opt/dmem", false).unwrap(), "must report nothing changed");
        assert!(!ws.join("cowork_plugins").exists(), "no tree written when refusing");
        assert_eq!(std::fs::read_to_string(ws.join("cowork_settings.json")).unwrap(), "{not json", "app file untouched");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cowork_remove_on_a_workspace_never_creates_app_files() {
        let ws = scratch("cowork-rm-ws");
        assert!(cowork_local_install(&ws, "/opt/dmem", true).unwrap());
        assert!(!ws.join("cowork_settings.json").exists());
        assert!(!ws.join("cowork_plugins").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
