//! Hook handlers invoked by the host agent (Devin, Claude Code) on lifecycle events.
//! They emit a Claude-Code-compatible additionalContext payload on stdout, which Devin
//! (CC-compatible hooks) and Claude Code both inject into the model context.

use crate::render;
use crate::tools::Memory;
use anyhow::Result;
use std::io::Read;

/// Emit a CC-compatible hook injection. Empty text = inject nothing (turn proceeds).
fn emit(event: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let out = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": event, "additionalContext": text }
    });
    println!("{}", out);
}

/// Read the hook event JSON from stdin and pull out a field (best-effort).
fn stdin_field(field: &str) -> Option<String> {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    v.get(field).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// SessionStart: inject persona/protocol + recent context.
pub fn session_start() -> Result<()> {
    let m = Memory::open()?;
    let persona = m.persona().unwrap_or_default();
    let recent = m.recent(5).unwrap_or_default();
    emit("SessionStart", &render::render_session(&persona, &recent));
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

/// SessionEnd/Stop: surface a save-discipline nudge if this session's work looks uncaptured.
/// Fail-open: on any error, or when there is nothing to nudge, emit nothing and proceed.
pub fn session_end() -> Result<()> {
    let m = match Memory::open() {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let latest = m.recent(1).ok().and_then(|v| v.first().map(|e| e.created_ms));
    if should_nudge(latest, crate::entry::now_ms()) {
        emit("SessionEnd", &render::render_nudge());
    }
    Ok(())
}

/// UserPromptSubmit: recall relevant memory for the submitted prompt (from stdin, or arg).
pub fn user_prompt_submit(arg: Option<String>) -> Result<()> {
    let prompt = arg
        .filter(|s| !s.trim().is_empty())
        .or_else(|| stdin_field("prompt"))
        .or_else(|| stdin_field("user_prompt"))
        .unwrap_or_default();
    if prompt.trim().len() < 3 {
        return Ok(());
    }
    let m = Memory::open()?;
    let hits = m.recall(&prompt, 6).unwrap_or_default();
    emit("UserPromptSubmit", &render::render_recall(&hits));
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
}
