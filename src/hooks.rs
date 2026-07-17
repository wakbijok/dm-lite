//! Hook handlers invoked by the host agent (Devin, Claude Code, Codex, Hermes, OpenCode) on
//! lifecycle events. By default they emit a Claude-Code-compatible additionalContext payload on
//! stdout, which Devin, Claude Code, and Codex (same hook shape) inject into the model context.
//! With `--hermes` they emit Hermes's `{"context": ...}` shape and read Hermes's hook-input
//! fields instead - Hermes has no context-injecting SessionStart, so persona rides the first
//! pre_llm_call (see `user_prompt_submit`). With `--raw` they emit the text verbatim (no JSON
//! envelope) for hosts that inject strings directly - the OpenCode plugin pushes hook output
//! onto the system prompt as-is.

use crate::render;
use crate::tools::Memory;
use anyhow::Result;
use std::io::Read;

/// Format a hook injection in the host's shape. None = inject nothing (turn proceeds).
fn format_output(event: &str, text: &str, hermes: bool, raw: bool) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    Some(if raw {
        text.to_string()
    } else if hermes {
        serde_json::json!({ "context": text }).to_string()
    } else {
        serde_json::json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } })
            .to_string()
    })
}

/// Emit a hook injection on stdout. Empty text = inject nothing.
fn emit(event: &str, text: &str, hermes: bool, raw: bool) {
    if let Some(out) = format_output(event, text, hermes, raw) {
        println!("{}", out);
    }
}

/// Read the hook event JSON from stdin once (best-effort), returning both the raw text (for
/// debug capture) and the parsed value. Callers pull fields off the parsed result.
fn read_stdin() -> (String, Option<serde_json::Value>) {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return (raw, None);
    }
    let parsed = serde_json::from_str(raw.trim()).ok();
    (raw, parsed)
}

/// Gated ground-truth capture for hook debugging: when DM_HOOK_DEBUG is set, append one JSON
/// line per invocation (raw stdin the host sent + what we parsed/emitted). DM_HOOK_DEBUG=1 logs
/// to <tmp>/dmem-hook-debug.log; any other value is treated as the log file path. Off by default.
fn debug_log(event: &str, hermes: bool, raw_stdin: &str, prompt: &str, first_turn: bool, emitted_len: usize) {
    let Some(spec) = std::env::var("DM_HOOK_DEBUG").ok().filter(|s| !s.is_empty()) else {
        return;
    };
    let path = if spec == "1" {
        std::env::temp_dir().join("dmem-hook-debug.log")
    } else {
        std::path::PathBuf::from(spec)
    };
    let rec = serde_json::json!({
        "ts_ms": crate::entry::now_ms(),
        "event": event,
        "hermes": hermes,
        "raw_stdin": raw_stdin.chars().take(2000).collect::<String>(),
        "parsed_prompt": prompt.chars().take(300).collect::<String>(),
        "is_first_turn": first_turn,
        "emitted_len": emitted_len,
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{rec}");
    }
}

/// SessionStart: inject persona/protocol + a lean open-reminders greet. Recent/recalled memory
/// rides the per-prompt UserPromptSubmit hook, NOT here, so the payload stays under Claude Code's
/// 10,000-char hook-stdout cap (see render::SESSION_BUDGET; over the cap CC persists the block to
/// a file and injects only a ~2KB preview, dropping the protocols from live context).
pub fn session_start(hermes: bool, raw: bool) -> Result<()> {
    let m = Memory::open()?;
    let persona = m.persona().unwrap_or_default();
    let reminders = m.reminders(5).unwrap_or_default();
    // Raw consumers inject straight into a system prompt, so use the plain-markdown bootstrap
    // projection (no injection fence); hook-JSON consumers keep the fenced session block.
    let text = if raw {
        render::render_bootstrap(&persona, &reminders)
    } else {
        render::render_session(&persona, &reminders)
    };
    emit("SessionStart", &text, hermes, raw);
    Ok(())
}

/// Quiet window after which an unsaved session looks like uncaptured work worth nudging.
const NUDGE_GAP_MS: i64 = 30 * 60_000; // 30 minutes

/// Whether to emit a save-discipline nudge at session end: nudge if nothing has been saved
/// at all, or the most recent save is older than the quiet window. Pure + deterministic.
fn should_nudge(latest_save_ms: Option<i64>, now_ms: i64) -> bool {
    match latest_save_ms {
        None => true,
        Some(ts) => now_ms.saturating_sub(ts) > NUDGE_GAP_MS,
    }
}

/// SessionEnd: a no-op for hook-JSON hosts - Claude Code's SessionEnd schema forbids injecting
/// context (the session is ending), so there the save-discipline nudge rides UserPromptSubmit
/// instead (see `user_prompt_submit`), and any older wiring that still calls it exits cleanly
/// with no output. With `--raw` it becomes a real end-of-session check for hosts that CAN act
/// on it (the OpenCode plugin calls it on session.idle and shows a toast): print the nudge when
/// this session's work looks uncaptured, nothing otherwise.
pub fn session_end(raw: bool) -> Result<()> {
    if !raw {
        return Ok(());
    }
    if !crate::config::save_nudge_enabled() {
        return Ok(());
    }
    let m = Memory::open()?;
    let latest = m.latest_save_ms().ok().flatten();
    if should_nudge(latest, crate::entry::now_ms()) {
        emit("SessionEnd", &render::render_nudge(), false, true);
    }
    Ok(())
}

/// UserPromptSubmit (Claude/Codex) / pre_llm_call (Hermes): recall relevant memory for the
/// submitted prompt and append a save-discipline nudge when this session's work looks
/// uncaptured. Claude/Codex put the prompt at top-level `prompt`; Hermes passes it as
/// `extra.user_message` (is_first_turn is still captured for diagnostics). For Hermes the
/// persona/protocols are NOT injected here; they live in SOUL.md (always-on system prompt).
pub fn user_prompt_submit(arg: Option<String>, hermes: bool, raw: bool) -> Result<()> {
    let (raw_in, input) = read_stdin();
    let prompt = arg
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let v = input.as_ref()?;
            if hermes {
                v.pointer("/extra/user_message").and_then(|x| x.as_str()).map(|s| s.to_string())
            } else {
                v.get("prompt").or_else(|| v.get("user_prompt")).and_then(|x| x.as_str()).map(|s| s.to_string())
            }
        })
        .unwrap_or_default();
    // Hermes flags the first turn so the persona/recent block can ride pre_llm_call (its
    // on_session_start hook cannot inject context).
    let first_turn = hermes
        && input
            .as_ref()
            .and_then(|v| v.pointer("/extra/is_first_turn"))
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
    if prompt.trim().len() < 3 {
        debug_log("user_prompt_submit", hermes, &raw_in, &prompt, first_turn, 0);
        return Ok(());
    }
    let m = Memory::open()?;
    let mut blocks: Vec<String> = Vec::new();
    // Persona/protocols are NOT injected here for Hermes: they live in SOUL.md (the always-on
    // system prompt, written by `dmem bootstrap --hermes`), so identity survives session resume
    // and compaction. This hook carries only the dynamic per-prompt layer: recall + save nudge.
    // Graph-augmented recall: pull the floor-trimmed seeds AND their 1-hop neighborhood, so
    // connected-but-not-similar context rides along (only where edges exist; lean otherwise).
    // Depth is env-dialable via DM_RECALL_EXPAND (default 1; 0 = plain recall).
    let depth = crate::config::recall_expand_depth();
    let plain = |what: &str| {
        m.recall(&prompt, 6).unwrap_or_else(|e| {
            eprintln!("dmem hook: {what} recall failed ({e:#}); injecting nothing");
            Vec::new()
        })
    };
    // A degraded graph path must NOT take recall down with it: on expansion failure (server
    // skew, transient network) fall back to the plain seeds and say so on stderr, instead of
    // silently injecting nothing - "memory had nothing to say" and "memory is broken" must
    // stay distinguishable.
    let (seeds, neighbors) = if depth == 0 {
        (plain("plain"), Vec::new())
    } else {
        match m.recall_expanded_split(&prompt, 6, depth) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("dmem hook: graph-expanded recall failed ({e:#}); falling back to plain recall");
                (plain("fallback plain"), Vec::new())
            }
        }
    };
    let recall = render::render_recall_split(&seeds, &neighbors);
    if !recall.trim().is_empty() {
        blocks.push(recall);
    }
    // cadence backstop: if nothing has been saved recently, remind to capture durable work.
    // Use the newest SAVE time (latest_save_ms), not recent(1): recent() orders by importance, so
    // it returns a persona/protocol record (importance 95) whose old timestamp made the nudge fire
    // every turn regardless of recent activity. Opt-in via DM_SAVE_NUDGE (default off): the save
    // discipline already rides every session's protocol layer, so this was a redundant copy.
    if crate::config::save_nudge_enabled() {
        let latest = m.latest_save_ms().ok().flatten();
        if should_nudge(latest, crate::entry::now_ms()) {
            blocks.push(render::render_nudge());
        }
    }
    let text = blocks.join("\n");
    debug_log("user_prompt_submit", hermes, &raw_in, &prompt, first_turn, text.len());
    emit("UserPromptSubmit", &text, hermes, raw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudges_when_nothing_saved_or_stale() {
        let now = 100 * 60_000; // 100 min in
        assert!(should_nudge(None, now), "no saves -> nudge");
        assert!(should_nudge(Some(now - 31 * 60_000), now), "stale (>30m) -> nudge");
        assert!(!should_nudge(Some(now - 5 * 60_000), now), "fresh (<30m) -> no nudge");
    }

    #[test]
    fn format_output_shapes_per_host() {
        // default: Claude-Code hook JSON envelope
        let cc = format_output("SessionStart", "hello", false, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&cc).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hello");
        // hermes: {"context": ...}
        let he = format_output("SessionStart", "hello", true, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&he).unwrap();
        assert_eq!(v["context"], "hello");
        // raw: the text verbatim, NOT JSON (quotes/newlines must survive unescaped)
        let text = "line one\nsays \"hi\"";
        assert_eq!(format_output("SessionStart", text, false, true).unwrap(), text);
    }

    #[test]
    fn format_output_empty_text_injects_nothing() {
        assert!(format_output("SessionStart", "", false, false).is_none());
        assert!(format_output("SessionStart", "  \n ", false, true).is_none());
        assert!(format_output("SessionStart", "", true, false).is_none());
    }

    #[test]
    fn nudge_cadence_boundary_and_clock_skew() {
        let now = 100 * 60_000;
        assert!(!should_nudge(Some(now - NUDGE_GAP_MS), now), "exactly 30m -> no nudge (boundary is exclusive)");
        assert!(should_nudge(Some(now - NUDGE_GAP_MS - 1), now), "30m + 1ms -> nudge");
        assert!(!should_nudge(Some(now), now), "just saved -> no nudge");
        assert!(!should_nudge(Some(now + 5), now), "future save (clock skew) -> no nudge (saturating_sub)");
    }
}
