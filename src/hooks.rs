//! Hook handlers invoked by the host agent (Devin, Claude Code, Claude Cowork, Codex, Hermes,
//! OpenCode) on lifecycle events. By default they emit a Claude-Code-compatible additionalContext
//! payload on stdout, which Devin, Claude Code, Cowork (same engine, via the generated plugin),
//! and Codex (same hook shape) inject into the model context.
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
    // A hook run by hand from a terminal has no event to read: do not wait for one.
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return (raw, None);
    }
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
    let (_, input) = read_stdin();
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
    let text = match work_state_line(input.as_ref()) {
        Some(line) if !text.trim().is_empty() => format!("{text}\n{line}"),
        Some(line) => line,
        None => text,
    };
    emit("SessionStart", &text, hermes, raw);
    Ok(())
}

/// The session-start work-state line for the session's repository (the event's `cwd`, else
/// the hook's own working directory); None outside a repository or with `DM_PROJECT=off`.
fn work_state_line(v: Option<&serde_json::Value>) -> Option<String> {
    if std::env::var(crate::project::PROJECT_ENV).is_ok_and(|p| ["off", "0", "none", "false"].iter().any(|o| p.trim().eq_ignore_ascii_case(o))) {
        return None;
    }
    let cwd = v.and_then(|v| v.get("cwd")).and_then(|c| c.as_str()).map(str::trim).filter(|c| !c.is_empty());
    let dir = match cwd {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir().ok()?,
    };
    crate::project::work_state(&dir).map(|w| w.line())
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

/// Pull the submitted prompt out of a hook-event JSON body. Hermes shell hooks
/// put it at `extra.user_message` (docs); some surfaces (and hand tests) send
/// top-level `user_message` / `prompt`. An empty parse must not be treated as
/// "memory had nothing to say" - that is how desktop awareness went silent.
fn prompt_from_hook_input(v: &serde_json::Value, hermes: bool) -> Option<String> {
    let paths: &[&str] = if hermes {
        &[
            "/extra/user_message",
            "/user_message",
            "/extra/prompt",
            "/prompt",
            "/user_prompt",
        ]
    } else {
        &["/prompt", "/user_prompt", "/extra/user_message", "/user_message"]
    };
    for p in paths {
        if let Some(s) = v.pointer(p).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
    }
    None
}

/// The project the hook's session works in: the event's `cwd` (Claude Code, Codex and the
/// Cowork plugin send the session's working directory with every event), else the hook
/// process's own working directory.
fn project_from_hook_input(v: Option<&serde_json::Value>) -> Option<String> {
    let cwd = v.and_then(|v| v.get("cwd")).and_then(|c| c.as_str()).map(str::trim).filter(|c| !c.is_empty());
    match cwd {
        Some(dir) => crate::project::detect(std::path::Path::new(dir)),
        None => crate::project::detect_cwd(),
    }
}

fn first_turn_from_hook_input(v: &serde_json::Value) -> bool {
    v.pointer("/extra/is_first_turn")
        .or_else(|| v.get("is_first_turn"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
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
        .or_else(|| input.as_ref().and_then(|v| prompt_from_hook_input(v, hermes)))
        .unwrap_or_default();
    // Hermes flags the first turn so the persona/recent block can ride pre_llm_call (its
    // on_session_start hook cannot inject context). Diagnostic only - recall runs every turn.
    let first_turn = hermes && input.as_ref().map(first_turn_from_hook_input).unwrap_or(false);
    if prompt.trim().len() < 3 {
        debug_log("user_prompt_submit", hermes, &raw_in, &prompt, first_turn, 0);
        return Ok(());
    }
    let mut m = Memory::open()?;
    m.set_project(project_from_hook_input(input.as_ref()));
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
    let graph = if depth == 0 {
        crate::tools::RecallGraph { seeds: plain("plain"), riders: Vec::new(), links: Vec::new() }
    } else {
        match m.recall_expanded_graph(&prompt, 6, depth) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("dmem hook: graph-expanded recall failed ({e:#}); falling back to plain recall");
                crate::tools::RecallGraph { seeds: plain("fallback plain"), riders: Vec::new(), links: Vec::new() }
            }
        }
    };
    let recall = render::render_recall_graph(&graph);
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

/// The most records one pre-edit injection carries: a few precise notes, not a recall block.
const PRE_EDIT_MAX: usize = 3;

/// The file an edit tool is about to change: `tool_input.file_path` (Edit, Write, MultiEdit),
/// else `tool_input.notebook_path` (NotebookEdit).
fn edited_file(v: &serde_json::Value) -> Option<String> {
    ["/tool_input/file_path", "/tool_input/notebook_path"]
        .iter()
        .find_map(|p| v.pointer(p).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
}

/// Where this session's already-shown pre-edit notes are listed (one uri per line), so a record
/// is injected once per session, not on every edit of the file. None without a session id.
fn seen_path(v: &serde_json::Value) -> Option<std::path::PathBuf> {
    let sid = v.get("session_id")?.as_str()?;
    let safe: String = sid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(80).collect();
    (!safe.is_empty()).then(|| std::env::temp_dir().join(format!("dmem-pre-edit-{safe}.seen")))
}

/// PreToolUse on the edit tools: before a file changes, inject what memory holds about that
/// file - records of its project that name its path or a distinctive folder of it. Precise or
/// silent: nothing about the file means no output, and the tool call proceeds untouched (no
/// permission decision is ever made here). Fails open: any error injects nothing.
pub fn pre_tool_use(raw: bool) -> Result<()> {
    let (raw_in, input) = read_stdin();
    let Some(v) = input else { return Ok(()) };
    let Some(file) = edited_file(&v) else { return Ok(()) };
    let cwd = v.get("cwd").and_then(|c| c.as_str()).map(std::path::Path::new);
    let Some(ctx) = crate::project::file_context(std::path::Path::new(&file), cwd) else { return Ok(()) };
    if ctx.project.is_none() {
        return Ok(());
    }
    let mut m = match Memory::open() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("dmem hook: pre-edit memory unavailable ({e:#}); injecting nothing");
            return Ok(());
        }
    };
    m.set_project(ctx.project.clone());
    // Keyword relevance without the access-signal bump: an edit is not a read, so it must not
    // inflate the signals recall ranks by. (Not the as-of path: that one orders by importance
    // and matches any term as a substring, so busy stores fill the window with persona records.)
    let hits = m.recall_quiet(&crate::project::file_query(&ctx), 40).unwrap_or_else(|e| {
        eprintln!("dmem hook: pre-edit recall failed ({e:#}); injecting nothing");
        Vec::new()
    });
    let seen_file = seen_path(&v);
    let seen: std::collections::HashSet<String> =
        seen_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default();
    let notes: Vec<crate::entry::Entry> = hits
        .into_iter()
        .filter(|e| !seen.contains(&e.uri))
        .filter(|e| {
            let text = format!("{}\n{}\n{}", e.title, e.body, e.tags.join(" "));
            crate::project::belongs(&e.tags, &text, &ctx) && crate::project::mentions_file(&text, &ctx.rel)
        })
        .take(PRE_EDIT_MAX)
        .collect();
    if notes.is_empty() {
        debug_log("pre_tool_use", false, &raw_in, &ctx.rel, false, 0);
        return Ok(());
    }
    if let Some(p) = &seen_file {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            for e in &notes {
                let _ = writeln!(f, "{}", e.uri);
            }
        }
    }
    let text = render::render_file_notes(&ctx.rel, &notes);
    debug_log("pre_tool_use", false, &raw_in, &ctx.rel, false, text.len());
    emit("PreToolUse", &text, false, raw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edited_file_comes_from_file_path_or_notebook_path() {
        let edit = serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": "/r/src/a.rs", "old_string": "x"}});
        assert_eq!(edited_file(&edit), Some("/r/src/a.rs".into()));
        let nb = serde_json::json!({"tool_name": "NotebookEdit", "tool_input": {"notebook_path": "/r/n.ipynb"}});
        assert_eq!(edited_file(&nb), Some("/r/n.ipynb".into()));
        assert_eq!(edited_file(&serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}})), None);
        assert_eq!(edited_file(&serde_json::json!({"tool_input": {"file_path": "  "}})), None);
    }

    #[test]
    fn the_seen_list_is_per_session_and_path_safe() {
        let a = seen_path(&serde_json::json!({"session_id": "abc-123"})).unwrap();
        let b = seen_path(&serde_json::json!({"session_id": "def-456"})).unwrap();
        assert_ne!(a, b);
        let hostile = seen_path(&serde_json::json!({"session_id": "../../etc/passwd"})).unwrap();
        assert_eq!(hostile.parent().unwrap(), std::env::temp_dir(), "no traversal out of the temp dir");
        assert!(seen_path(&serde_json::json!({})).is_none());
        assert!(seen_path(&serde_json::json!({"session_id": "///"})).is_none());
    }

    #[test]
    fn pre_edit_output_is_an_additional_context_with_no_permission_decision() {
        let out = format_output("PreToolUse", "note", false, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "note");
        assert!(v["hookSpecificOutput"].get("permissionDecision").is_none(), "never approves or denies the edit");
    }

    #[test]
    fn the_events_cwd_names_the_project() {
        let base = std::env::temp_dir().join(format!("dmhookproj-{}-{}", std::process::id(), crate::entry::now_ms()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().map(|o| o.status.success()).unwrap_or(false);
        if git(&["init", "-q"]) && git(&["remote", "add", "origin", "https://u:secret@git.example.org/Team/App.git"]) && std::env::var(crate::project::PROJECT_ENV).is_err() {
            let v = serde_json::json!({"prompt": "x", "cwd": repo.to_str().unwrap()});
            assert_eq!(project_from_hook_input(Some(&v)), Some("git.example.org/team/app".into()));
            let outside = serde_json::json!({"prompt": "x", "cwd": base.to_str().unwrap()});
            assert_eq!(project_from_hook_input(Some(&outside)), None);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

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

    #[test]
    fn hermes_prompt_accepts_extra_and_top_level() {
        let extra = serde_json::json!({"extra": {"user_message": "  wg ip  ", "is_first_turn": true}});
        assert_eq!(prompt_from_hook_input(&extra, true).as_deref(), Some("wg ip"));
        assert!(first_turn_from_hook_input(&extra));

        let top = serde_json::json!({"user_message": "desktop turn", "is_first_turn": false});
        assert_eq!(
            prompt_from_hook_input(&top, true).as_deref(),
            Some("desktop turn")
        );
        assert!(!first_turn_from_hook_input(&top));

        let empty = serde_json::json!({"extra": {"user_message": "  "}});
        assert!(prompt_from_hook_input(&empty, true).is_none());
    }

    #[test]
    fn claude_prompt_still_prefers_top_level_prompt() {
        let v = serde_json::json!({"prompt": "cc prompt", "user_message": "ignored"});
        assert_eq!(prompt_from_hook_input(&v, false).as_deref(), Some("cc prompt"));
    }
}
