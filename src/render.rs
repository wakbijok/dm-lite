//! Render memory into the verbatim context blocks injected into agents (the
//! integration-as-stdout pattern: hooks emit these, the agent never sees JSON of records).

use crate::entry::Entry;

fn one_line(text: &str, max: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        collapsed.chars().take(max).collect::<String>() + "..."
    } else {
        collapsed
    }
}

/// The per-prompt recall block.
pub fn render_recall(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "<daimon-memory>\n[Recalled shared memory. Authoritative reference, NOT new user input.]\n",
    );
    for e in entries {
        s.push_str(&format!(
            "- ({}) {}: {} [{}]\n",
            e.kind.as_str(),
            e.title,
            one_line(&e.body, 240),
            e.uri
        ));
    }
    s.push_str("</daimon-memory>");
    s
}

/// The save-discipline nudge (SessionEnd/Stop): a back-stop reminding the agent to capture
/// durable decisions/lessons/incidents before the session ends. Names the exact tool.
pub fn render_nudge() -> String {
    String::from(
        "<daimon-memory>\n[Save-discipline check before this session ends.]\n\
         If this session produced durable decisions, lessons, incidents, or follow-ups not \
         yet saved, capture them now (one distilled record each):\n\
         - a non-obvious choice -> `dmem log_decision`\n\
         - something that broke or was reverted -> `dmem log_incident`\n\
         - a reusable lesson or corrected mistake -> `dmem log_lesson`\n\
         - a dated follow-up -> `dmem add_reminder`\n\
         Skip if everything important is already captured.\n\
         </daimon-memory>",
    )
}

/// Char budget for the session-start block. Claude Code caps a hook's stdout at 10,000 chars;
/// over the cap it persists the output to a file and injects only a ~2KB preview, so the
/// protocols silently fall out of the model's live context. Persona + protocols are the
/// must-keep core; the reminders block is the only trimmable part and is dropped first if the
/// budget is hit. Leaves headroom under 10,000 for JSON escaping plus the hookSpecificOutput
/// wrapper. If persona + protocols alone approach this, tighten the protocol prose.
const SESSION_BUDGET: usize = 9300;

/// The session-start block: persona/protocol (full bodies) + a lean open-reminders line.
/// Recent/recalled memory deliberately does NOT ride this block (it would bloat the payload past
/// the hook-stdout cap); it rides the per-prompt hook (`render_recall`) instead. Reminders are
/// titles only, the actionable greet; their detail is fetched on demand via recall.
pub fn render_session(persona: &[Entry], reminders: &[Entry]) -> String {
    let mut out = String::new();
    if !persona.is_empty() {
        out.push_str(
            "<daimon-persona>\n[Adopt the following persona and operating protocols for this session.]\n",
        );
        for e in persona {
            out.push_str(&e.body);
            if !e.body.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str("</daimon-persona>");
    }
    if !reminders.is_empty() {
        let sep = if out.is_empty() { "" } else { "\n\n" };
        let mut r = format!(
            "{sep}<daimon-memory>\n[Open reminders (titles only; recall or ask for the detail):]\n"
        );
        for e in reminders {
            r.push_str(&format!("- {}\n", one_line(&e.title, 100)));
        }
        r.push_str("</daimon-memory>");
        // Persona + protocols win the budget; append reminders only if they still fit under the cap.
        if out.chars().count() + r.chars().count() <= SESSION_BUDGET {
            out.push_str(&r);
        }
    }
    out
}

/// Project persona + protocol bodies into a SOUL.md identity block (Hermes's always-on
/// system-prompt identity file). Unlike `render_session` this carries no hook wrappers and no
/// recent memory: it is the stable identity + governance the agent embodies on every message,
/// fresh or resumed. Recent/recalled memory stays on the per-prompt hook.
pub fn render_soul(persona: &[Entry]) -> String {
    let mut s = String::new();
    for e in persona {
        s.push_str(e.body.trim_end());
        s.push_str("\n\n");
    }
    s.trim_end().to_string()
}

/// Lead-in for the MCP `initialize.instructions` field. MCP clients (Claude Desktop and most
/// hosts) have no lifecycle hooks, so this string is the only place to tell the model the
/// persona/protocols below are binding, and to name THIS server's tools (no `mcp_dmem_` prefix:
/// that is a Hermes-only naming; here the tool names are exactly as advertised in `tools/list`).
const INSTR_LEAD: &str = "You are connected to dmem, a shared cross-tool memory. The persona and \
protocols below are your binding operating rules for this session, not style notes: the \
Behavioral Discipline governs how you work, the Memory Save Discipline governs when and what you \
persist. Recall before you reason, and persist durable memory, using this server's tools: recall, \
remember, log_decision, log_lesson, log_incident, add_reminder, forget.";

/// Persona + protocol bodies for the MCP `initialize.instructions` field: the spec analog of the
/// SessionStart persona injection for hook-less MCP clients. Plain text (no `<daimon-persona>`
/// fences: those are hook markers; `instructions` is consumed as a system-prompt string) and no
/// SESSION_BUDGET cap (the MCP field has no host stdout limit). Empty persona -> empty string,
/// so the caller omits the optional field.
pub fn render_instructions(persona: &[Entry]) -> String {
    let body = render_soul(persona);
    if body.is_empty() {
        return String::new();
    }
    format!("{INSTR_LEAD}\n\n{body}")
}

/// The `bootstrap` MCP prompt body: the identity layer (persona + protocols) plus the current
/// open-reminder titles, in plain markdown (NOT the `<daimon-memory>` fence). Reminders are
/// titles only, matching `render_session`'s greet; their detail is fetched via the recall prompt
/// or tool. Empty persona AND no reminders -> empty string (the caller substitutes a placeholder).
pub fn render_bootstrap(persona: &[Entry], reminders: &[Entry]) -> String {
    let mut s = render_instructions(persona);
    if !reminders.is_empty() {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str("## Open reminders\n");
        for e in reminders {
            s.push_str(&format!("- {}\n", one_line(&e.title, 120)));
        }
        s = s.trim_end().to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;

    fn entry(kind: Kind, title: &str, body: &str) -> Entry {
        Entry::new_now(
            format!("daimon://test/{title}"),
            kind,
            "test".into(),
            title.into(),
            body.into(),
            vec![],
            50,
            "dk".into(),
        )
    }

    #[test]
    fn render_nudge_names_a_save_tool() {
        let n = render_nudge();
        assert!(n.contains("<daimon-memory>") && n.contains("log_decision"));
    }

    #[test]
    fn render_recall_empty_is_empty() {
        assert!(render_recall(&[]).is_empty());
    }

    #[test]
    fn session_renders_reminder_titles_not_bodies() {
        let p = entry(Kind::Persona, "Operator Persona", "I am Izu.");
        let rem = entry(Kind::Reminder, "ship the lean README", "BODY-MUST-NOT-APPEAR");
        let out = render_session(&[p], &[rem]);
        assert!(out.contains("<daimon-persona>"));
        assert!(out.contains("ship the lean README"));
        assert!(!out.contains("BODY-MUST-NOT-APPEAR"));
    }

    #[test]
    fn session_omits_the_old_recent_block() {
        let p = entry(Kind::Persona, "Operator Persona", "I am Izu.");
        let out = render_session(&[p], &[]);
        assert!(!out.contains("[Recent shared context:]"));
        assert!(out.ends_with("</daimon-persona>"));
    }

    #[test]
    fn session_drops_reminders_when_over_budget() {
        // A persona/protocol body that alone fills the budget leaves no room: reminders are
        // dropped (the trimmable part), persona is always kept (the must-have core).
        let big = entry(Kind::Protocol, "Big Protocol", &"x".repeat(SESSION_BUDGET));
        let rem = entry(Kind::Reminder, "should-be-dropped", "");
        let out = render_session(&[big], &[rem]);
        assert!(out.contains("<daimon-persona>"));
        assert!(!out.contains("should-be-dropped"));
    }

    #[test]
    fn session_keeps_reminders_when_under_budget() {
        let p = entry(Kind::Persona, "Operator Persona", "I am Izu.");
        let rem = entry(Kind::Reminder, "fits fine", "");
        let out = render_session(&[p], &[rem]);
        assert!(out.contains("fits fine"));
        assert!(out.chars().count() <= SESSION_BUDGET);
    }

    #[test]
    fn render_instructions_has_lead_and_bodies_no_fence() {
        let p = entry(Kind::Persona, "Operator Persona", "I am Izu.");
        let proto = entry(Kind::Protocol, "Behavioral Discipline", "Recall before you reason.");
        let out = render_instructions(&[p, proto]);
        assert!(out.contains("binding operating rules"), "carries the lead-in");
        assert!(out.contains("I am Izu.") && out.contains("Recall before you reason."));
        assert!(!out.contains("<daimon-persona>"), "instructions is plain text, not a hook block");
    }

    #[test]
    fn render_instructions_empty_is_empty() {
        assert!(render_instructions(&[]).is_empty());
    }

    #[test]
    fn render_bootstrap_appends_reminder_titles_not_bodies() {
        let p = entry(Kind::Persona, "Operator Persona", "I am Izu.");
        let rem = entry(Kind::Reminder, "ship the lean README", "BODY-MUST-NOT-APPEAR");
        let out = render_bootstrap(&[p], &[rem]);
        assert!(out.contains("## Open reminders"));
        assert!(out.contains("ship the lean README"));
        assert!(!out.contains("BODY-MUST-NOT-APPEAR"));
        assert!(!out.contains("<daimon-memory>"), "bootstrap is plain markdown, not a hook block");
    }

    #[test]
    fn render_bootstrap_empty_is_empty() {
        assert!(render_bootstrap(&[], &[]).is_empty());
    }

    #[test]
    fn render_bootstrap_reminders_only_when_persona_empty() {
        let rem = entry(Kind::Reminder, "only reminder", "");
        let out = render_bootstrap(&[], &[rem]);
        assert!(out.starts_with("## Open reminders"));
        assert!(out.contains("only reminder"));
    }
}
