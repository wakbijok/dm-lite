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
