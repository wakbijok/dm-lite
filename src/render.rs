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

/// The session-start block: persona/protocol (full bodies) + recent context.
pub fn render_session(persona: &[Entry], recent: &[Entry]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !persona.is_empty() {
        let mut p = String::from(
            "<daimon-persona>\n[Adopt the following persona and operating protocols for this session.]\n",
        );
        for e in persona {
            p.push_str(&e.body);
            if !e.body.ends_with('\n') {
                p.push('\n');
            }
        }
        p.push_str("</daimon-persona>");
        parts.push(p);
    }
    let recent_block = if recent.is_empty() {
        String::new()
    } else {
        let mut r = String::from("<daimon-memory>\n[Recent shared context:]\n");
        for e in recent {
            r.push_str(&format!(
                "- ({}) {}: {} [{}]\n",
                e.kind.as_str(),
                e.title,
                one_line(&e.body, 200),
                e.uri
            ));
        }
        r.push_str("</daimon-memory>");
        r
    };
    if !recent_block.is_empty() {
        parts.push(recent_block);
    }
    parts.join("\n\n")
}
