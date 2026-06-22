//! The typed memory model (engine-agnostic). This is the v1 model carried into v2
//! unchanged: typed kinds, the daimon:// URI, a namespace, source text as canonical.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Memory record kinds. The exact set from daimon-memory v1 (so migration is 1:1). serde
/// emits the snake_case names, matching `as_str` and the v1 convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Decision,
    Runbook,
    IncidentSummary,
    ServiceTopology,
    KnownFailureMode,
    RemediationPattern,
    ProjectConvention,
    AgentLesson,
    ResourceSummary,
    Persona,
    Protocol,
    Reminder,
    Memory,
    // Domain-entity kinds (the knowledge-graph layer): the "things" the records are about.
    Org,
    Engagement,
    Product,
    SolutionStack,
    Person,
    Framework,
    Site,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Decision => "decision",
            Kind::Runbook => "runbook",
            Kind::IncidentSummary => "incident_summary",
            Kind::ServiceTopology => "service_topology",
            Kind::KnownFailureMode => "known_failure_mode",
            Kind::RemediationPattern => "remediation_pattern",
            Kind::ProjectConvention => "project_convention",
            Kind::AgentLesson => "agent_lesson",
            Kind::ResourceSummary => "resource_summary",
            Kind::Persona => "persona",
            Kind::Protocol => "protocol",
            Kind::Reminder => "reminder",
            Kind::Memory => "memory",
            Kind::Org => "org",
            Kind::Engagement => "engagement",
            Kind::Product => "product",
            Kind::SolutionStack => "solution_stack",
            Kind::Person => "person",
            Kind::Framework => "framework",
            Kind::Site => "site",
        }
    }

    /// Parse a kind. Accepts v1's canonical names (what dm-lite emits) plus the short aliases
    /// dm-lite briefly used, so older records still resolve.
    pub fn from_str(s: &str) -> Option<Kind> {
        Some(match s {
            "decision" => Kind::Decision,
            "runbook" => Kind::Runbook,
            "incident_summary" | "incident" => Kind::IncidentSummary,
            "service_topology" => Kind::ServiceTopology,
            "known_failure_mode" => Kind::KnownFailureMode,
            "remediation_pattern" => Kind::RemediationPattern,
            "project_convention" | "convention" => Kind::ProjectConvention,
            "agent_lesson" | "lesson" => Kind::AgentLesson,
            "resource_summary" => Kind::ResourceSummary,
            "persona" => Kind::Persona,
            "protocol" => Kind::Protocol,
            "reminder" => Kind::Reminder,
            "memory" => Kind::Memory,
            "org" => Kind::Org,
            "engagement" => Kind::Engagement,
            "product" => Kind::Product,
            "solution_stack" => Kind::SolutionStack,
            "person" => Kind::Person,
            "framework" => Kind::Framework,
            "site" => Kind::Site,
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

/// A directed, typed relation between two records: the graph layer over the memory. `from_uri`
/// and `to_uri` are record uris; `rel` is the relation type ("links", "supersedes", "informed",
/// "part-of", "about", and the entity relations such as "for", "uses", "made-by", "alias-of").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    pub from_uri: String,
    pub to_uri: String,
    pub rel: String,
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

/// Parse a template markdown file with a YAML-ish frontmatter header into
/// (kind, namespace, title, body). Only `kind`, `namespace`, `title` keys are read.
pub fn parse_frontmatter(text: &str) -> Result<(Kind, String, String, String), String> {
    let t = text.trim_start();
    let rest = t.strip_prefix("---").ok_or("missing frontmatter (expected a leading ---)")?;
    let end = rest.find("\n---").ok_or("unterminated frontmatter (expected a closing ---)")?;
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();

    let mut kind: Option<Kind> = None;
    let mut namespace = String::new();
    let mut title = String::new();
    for line in header.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.trim() {
                "kind" => kind = Kind::from_str(&val),
                "namespace" => namespace = val,
                "title" => title = val,
                _ => {}
            }
        }
    }
    let kind = kind.ok_or("frontmatter `kind` is missing or not a known kind")?;
    if title.trim().is_empty() {
        return Err("frontmatter `title` is required".into());
    }
    if namespace.trim().is_empty() {
        namespace = "resources/notes".into();
    }
    Ok((kind, namespace, title, body))
}

/// Default importance for an imported record by kind (persona/protocol rank highest).
pub fn default_importance(kind: Kind) -> i64 {
    match kind {
        Kind::Persona | Kind::Protocol => 95,
        Kind::ProjectConvention => 70,
        _ => 60,
    }
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
        // EVERY kind must round-trip through as_str -> from_str, the governance kinds
        // (Persona/Protocol) and Memory included: persona() and the MCP instructions field
        // depend on those resolving.
        for k in [
            Kind::Decision, Kind::Runbook, Kind::IncidentSummary, Kind::ServiceTopology,
            Kind::KnownFailureMode, Kind::RemediationPattern, Kind::ProjectConvention,
            Kind::AgentLesson, Kind::ResourceSummary, Kind::Persona, Kind::Protocol,
            Kind::Reminder, Kind::Memory,
            Kind::Org, Kind::Engagement, Kind::Product, Kind::SolutionStack,
            Kind::Person, Kind::Framework, Kind::Site,
        ] {
            assert_eq!(Kind::from_str(k.as_str()), Some(k), "kind {:?} must round-trip", k);
        }
        // the short aliases dm-lite briefly emitted still resolve
        assert_eq!(Kind::from_str("incident"), Some(Kind::IncidentSummary));
        assert_eq!(Kind::from_str("convention"), Some(Kind::ProjectConvention));
        assert_eq!(Kind::from_str("lesson"), Some(Kind::AgentLesson));
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
