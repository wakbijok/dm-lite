//! The typed memory model (engine-agnostic). This is the v1 model carried into v2
//! unchanged: typed kinds, the daimon:// URI, a namespace, source text as canonical.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Memory record kinds. A closed set; the open `extra` lives in tags/body for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Decision,
    Lesson,
    Incident,
    Runbook,
    Convention,
    Reminder,
    ResourceSummary,
    Persona,
    Protocol,
    Memory,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Decision => "decision",
            Kind::Lesson => "lesson",
            Kind::Incident => "incident",
            Kind::Runbook => "runbook",
            Kind::Convention => "convention",
            Kind::Reminder => "reminder",
            Kind::ResourceSummary => "resource_summary",
            Kind::Persona => "persona",
            Kind::Protocol => "protocol",
            Kind::Memory => "memory",
        }
    }

    pub fn from_str(s: &str) -> Option<Kind> {
        Some(match s {
            "decision" => Kind::Decision,
            "lesson" => Kind::Lesson,
            "incident" => Kind::Incident,
            "runbook" => Kind::Runbook,
            "convention" => Kind::Convention,
            "reminder" => Kind::Reminder,
            "resource_summary" => Kind::ResourceSummary,
            "persona" => Kind::Persona,
            "protocol" => Kind::Protocol,
            "memory" => Kind::Memory,
            _ => return None,
        })
    }
}

/// One memory record version. `body` is the canonical source text (vectors are a
/// rebuildable index derived from it). Bitemporal: two independent time axes.
/// - VALID time (`valid_from_ms`..`valid_to_ms`): when the fact is true *in the world*.
///   `valid_to_ms == None` means "still true".
/// - SYSTEM/transaction time (`system_from_ms`..`system_to_ms`): when this row version was
///   *recorded*. `system_to_ms == None` means "this is the currently-recorded version".
/// The store is append-only: superseding a record closes the prior version's system time
/// and inserts a new one; no version is ever destroyed. The "current slice" (default reads)
/// is `system_to_ms IS None AND (valid_to_ms IS None OR valid_to_ms > now)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub uri: String,
    pub kind: Kind,
    pub namespace: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub importance: i64,
    pub dedup_key: String,
    pub created_ms: i64,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub system_from_ms: i64,
    pub system_to_ms: Option<i64>,
}

impl Entry {
    /// A new live record recorded now and valid from now (both axes open-ended).
    /// The store assigns the authoritative `system_from_ms` on insert.
    pub fn new_now(
        uri: String,
        kind: Kind,
        namespace: String,
        title: String,
        body: String,
        tags: Vec<String>,
        importance: i64,
        dedup_key: String,
    ) -> Self {
        let now = now_ms();
        Entry {
            uri,
            kind,
            namespace,
            title,
            body,
            tags,
            importance,
            dedup_key,
            created_ms: now,
            valid_from_ms: now,
            valid_to_ms: None,
            system_from_ms: now,
            system_to_ms: None,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// kebab-case slug from a title, capped, for the daimon:// URI.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(60).collect()
}

/// daimon://<namespace>/<kind>/<slug>
pub fn make_uri(namespace: &str, kind: Kind, title: &str) -> String {
    format!("daimon://{}/{}/{}", namespace.trim_matches('/'), kind.as_str(), slug(title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_kebabs_and_caps() {
        assert_eq!(slug("Ride along on ma8e!"), "ride-along-on-ma8e");
        assert_eq!(slug("  weird __ spacing  "), "weird-spacing");
    }

    #[test]
    fn kind_roundtrips() {
        for k in [Kind::Decision, Kind::Lesson, Kind::Incident, Kind::Reminder, Kind::ResourceSummary] {
            assert_eq!(Kind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(Kind::from_str("nope"), None);
    }

    #[test]
    fn uri_shape() {
        let u = make_uri("resources/daimon-memory", Kind::Decision, "Lock LanceDB");
        assert_eq!(u, "daimon://resources/daimon-memory/decision/lock-lancedb");
    }

    #[test]
    fn new_now_is_open_ended_on_both_axes() {
        let e = Entry::new_now(
            "daimon://x".into(), Kind::Memory, "x".into(), "t".into(),
            "b".into(), vec![], 50, "daimon://x".into(),
        );
        assert!(e.valid_to_ms.is_none() && e.system_to_ms.is_none());
        assert_eq!(e.valid_from_ms, e.system_from_ms);
        assert_eq!(e.created_ms, e.system_from_ms);
        // round-trips through serde with the new fields
        let j = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&j).unwrap();
        assert_eq!(back.system_from_ms, e.system_from_ms);
        assert!(back.system_to_ms.is_none());
    }
}
