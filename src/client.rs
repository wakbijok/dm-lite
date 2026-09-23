//! Remote-client mode: talk to a remote `dmem serve` over HTTP(S) with a bearer token.
//! Selected when the config has a `[server]` block. Blocking reqwest (rustls), so the CLI and
//! hooks stay synchronous (no tokio). TLS is always verified: `ca_cert` pins a specific CA or
//! self-signed server cert; there is no switch to skip verification, because every request
//! carries the bearer token and an unverified channel would hand it to a MITM. The server
//! enforces tenant isolation; this client just carries the token.

use crate::config::ServerLink;
use crate::entry::{Edge, Entry, Kind};
use crate::tools::{RecallGraph, RecallRider};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

pub struct RemoteClient {
    base: String,
    token: String,
    http: reqwest::blocking::Client,
}

/// Why a config with `[server].insecure = true` is refused instead of silently ignored: the old
/// switch disabled certificate verification, which sent the bearer token to whoever answered.
/// Refusing with the fix spelled out beats an unexplained TLS failure from the hooks.
pub const INSECURE_REFUSED: &str = "[server].insecure is no longer honoured: it disabled TLS verification and sent the bearer token to whoever answered. \
Pin the server certificate instead: `dmem login <url> --ca-cert <cert.pem>` (`dmem serve --tls-generate` writes it under <data>/tls/cert.pem; \
start the server with `--tls-san <public hostname>` so the cert covers the name clients use), then delete `insecure` from the config.";

/// Hint appended to a request error when the failure is the server certificate not being
/// trusted (self-signed without `ca_cert`, or a cert that does not cover the hostname).
const TLS_TRUST_HINT: &str = "; the server certificate is not trusted: pin it with `dmem login <url> --ca-cert <cert.pem>` (see README, Security model)";

/// Walk an error's source chain looking for a rustls/webpki certificate-trust failure.
fn is_tls_trust_error(e: &dyn std::error::Error) -> bool {
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        let s = err.to_string();
        if s.contains("certificate") || s.contains("UnknownIssuer") || s.contains("NotValidForName") {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Wrap a transport error with the request label, keeping the source chain (so `main`'s `{:#}`
/// still walks reqwest -> connect -> rustls) and adding the pinning hint when it is a trust failure.
fn send_context(e: reqwest::Error, what: String) -> anyhow::Error {
    let hint = if is_tls_trust_error(&e) { TLS_TRUST_HINT } else { "" };
    anyhow::Error::new(e).context(format!("{what}{hint}"))
}

impl RemoteClient {
    pub fn new(link: &ServerLink) -> Result<Self> {
        // connect_timeout bounds the unreachable-server case: without it a packet-dropping
        // host eats the FULL per-prompt hook budget (8s) before Claude Code kills the hook.
        let mut b = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(3));
        if link.insecure {
            anyhow::bail!("{INSECURE_REFUSED}");
        }
        if let Some(ca) = &link.ca_cert {
            let pem = std::fs::read(ca).map_err(|e| anyhow!("read ca_cert {ca}: {e}"))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| anyhow!("parse ca_cert: {e}"))?;
            b = b.add_root_certificate(cert);
        }
        let http = b.build().context("build http client")?;
        Ok(Self {
            base: link.url.trim_end_matches('/').to_string(),
            token: link.token.clone(),
            http,
        })
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            // `send_context` (not `anyhow!("{e}")`) preserves the reqwest error as a source, so
            // `main`'s `{:#}` walks the whole chain (reqwest -> connect -> rustls -> root cause).
            .map_err(|e| send_context(e, format!("POST {path}")))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("server {} on {}: {}", status.as_u16(), path, text.trim());
        }
        // A 2xx with an undecodable body is a real error (broken channel / version skew), NOT an
        // empty result: coercing it to Null/empty would silently render empty governance and look
        // like a regression. Surface it. The body is not echoed (it could hold returned secrets).
        serde_json::from_str(&text).map_err(|e| anyhow!("decode response from {path}: {e}"))
    }

    fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| send_context(e, format!("GET {path}")))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("server {} on {}: {}", status.as_u16(), path, text.trim());
        }
        serde_json::from_str(&text).map_err(|e| anyhow!("decode response from {path}: {e}"))
    }

    fn list(&self, path: &str, body: Value) -> Result<Vec<Entry>> {
        let v = self.post(path, body)?;
        serde_json::from_value(v).map_err(|e| anyhow!("decode record list from {path}: {e}"))
    }

    // --- admin (root-token) operations ---

    #[allow(clippy::too_many_arguments)]
    pub fn admin_add(
        &self,
        tenant: &str,
        label: &str,
        display: &str,
        agent: Option<&str>,
        scope_read: &[String],
        scope_write: Option<&str>,
        adapter: bool,
    ) -> Result<(String, String)> {
        // Scope fields are additive on the wire: omitted entirely for a full-tenant token so
        // an older server never sees unknown-but-consequential fields.
        let mut body = json!({ "tenant": tenant, "label": label, "display": display, "agent": agent });
        if !scope_read.is_empty() || scope_write.is_some() || adapter {
            body["scope_read"] = json!(scope_read);
            body["scope_write"] = json!(scope_write);
            body["adapter"] = json!(adapter);
        }
        let v = self.post("/admin/tenant", body)?;
        Ok((
            v.get("tenant").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            v.get("token").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        ))
    }

    pub fn admin_list(&self) -> Result<Value> {
        self.get("/admin/tenants")
    }

    pub fn admin_revoke(&self, target: &str) -> Result<i64> {
        let v = self.post("/admin/revoke", json!({ "target": target }))?;
        Ok(v.get("revoked").and_then(|x| x.as_i64()).unwrap_or(0))
    }

    pub fn admin_rm(&self, tenant: &str) -> Result<()> {
        self.post("/admin/rm", json!({ "target": tenant }))?;
        Ok(())
    }

    fn uri_of(&self, path: &str, body: Value) -> Result<String> {
        let v = self.post(path, body)?;
        v.get("uri")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("server response missing uri"))
    }

    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        self.list("/recall", json!({ "query": query, "limit": limit }))
    }
    pub fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, valid_ms: i64) -> Result<Vec<Entry>> {
        // Carry BOTH bitemporal axes over the wire so remote as-of matches local (the server
        // defaults `valid` to `as_of` when absent, preserving older clients).
        self.list("/recall", json!({ "query": query, "limit": limit, "as_of": as_of_ms, "valid": valid_ms }))
    }
    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        self.list("/recent", json!({ "limit": limit }))
    }
    pub fn latest_save_ms(&self) -> Result<Option<i64>> {
        let v = self.post("/latest_save", json!({}))?;
        Ok(v.get("latest_save_ms").and_then(|x| x.as_i64()))
    }
    pub fn history(&self, uri: &str, limit: usize) -> Result<Vec<Entry>> {
        self.list("/history", json!({ "uri": uri, "limit": limit }))
    }
    pub fn forget(&self, uri: &str) -> Result<usize> {
        let v = self.post("/forget", json!({ "uri": uri }))?;
        Ok(v.get("forgotten").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
    }
    pub fn persona(&self) -> Result<Vec<Entry>> {
        self.list("/persona", json!({}))
    }
    pub fn reminders(&self, limit: usize) -> Result<Vec<Entry>> {
        self.list("/reminders", json!({ "limit": limit }))
    }
    pub fn counts(&self) -> Result<Vec<(String, usize)>> {
        // counts live server-side; a remote `dmem status` reports the connection, not tallies.
        Ok(Vec::new())
    }
    pub fn recall_mode(&self) -> &'static str {
        "remote (HTTP client -> dmem serve)"
    }
    pub fn remember(&self, text: &str, namespace: &str, valid_from: Option<i64>, valid_to: Option<i64>) -> Result<String> {
        self.uri_of("/remember", json!({ "text": text, "namespace": namespace, "valid_from": valid_from, "valid_to": valid_to }))
    }
    pub fn invalidate(&self, uri: &str, valid_to_ms: i64) -> Result<usize> {
        let v = self.post("/invalidate", json!({ "uri": uri, "valid_to": valid_to_ms }))?;
        Ok(v.get("invalidated").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
    }
    pub fn log_decision(&self, title: &str, context: &str, decision: &str, rationale: &str, namespace: &str) -> Result<String> {
        self.uri_of(
            "/log_decision",
            json!({ "title": title, "context": context, "decision": decision, "rationale": rationale, "namespace": namespace }),
        )
    }
    pub fn log_lesson(&self, title: &str, lesson: &str, namespace: &str) -> Result<String> {
        self.uri_of("/log_lesson", json!({ "title": title, "lesson": lesson, "namespace": namespace }))
    }
    pub fn log_incident(&self, title: &str, impact: &str, resolution: &str, namespace: &str) -> Result<String> {
        self.uri_of(
            "/log_incident",
            json!({ "title": title, "impact": impact, "resolution": resolution, "namespace": namespace }),
        )
    }
    pub fn add_reminder(&self, title: &str, text: &str, namespace: &str) -> Result<String> {
        self.uri_of("/add_reminder", json!({ "title": title, "text": text, "namespace": namespace }))
    }
    pub fn log_runbook(&self, title: &str, steps: &str, namespace: &str) -> Result<String> {
        self.uri_of("/log_runbook", json!({ "title": title, "steps": steps, "namespace": namespace }))
    }
    pub fn log_convention(&self, title: &str, rule: &str, namespace: &str) -> Result<String> {
        self.uri_of("/log_convention", json!({ "title": title, "rule": rule, "namespace": namespace }))
    }
    pub fn import_record(&self, kind: Kind, namespace: &str, title: &str, body: &str) -> Result<String> {
        self.uri_of(
            "/import",
            json!({ "kind": kind.as_str(), "namespace": namespace, "title": title, "body": body }),
        )
    }
    pub fn import_record_at(&self, kind: Kind, namespace: &str, title: &str, body: &str, created_ms: i64, importance: Option<i64>) -> Result<String> {
        self.uri_of(
            "/import",
            json!({ "kind": kind.as_str(), "namespace": namespace, "title": title, "body": body, "created_ms": created_ms, "importance": importance }),
        )
    }

    // --- graph layer ---

    pub fn link(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<()> {
        self.post("/link", json!({ "from": from_uri, "to": to_uri, "rel": rel }))?;
        Ok(())
    }
    pub fn unlink(&self, from_uri: &str, to_uri: &str, rel: &str) -> Result<usize> {
        let v = self.post("/unlink", json!({ "from": from_uri, "to": to_uri, "rel": rel }))?;
        Ok(v.get("unlinked").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
    }
    pub fn edges_of(&self, uri: &str) -> Result<Vec<Edge>> {
        let v = self.post("/edges", json!({ "uri": uri }))?;
        serde_json::from_value(v).map_err(|e| anyhow!("decode edges from /edges: {e}"))
    }
    pub fn all_edges(&self, limit: usize) -> Result<Vec<Edge>> {
        let v = self.post("/edges_all", json!({ "limit": limit }))?;
        serde_json::from_value(v).map_err(|e| anyhow!("decode edges from /edges_all: {e}"))
    }
    pub fn neighbors(&self, seeds: &[String], depth: usize, limit: usize) -> Result<Vec<String>> {
        let v = self.post("/neighbors", json!({ "seeds": seeds, "depth": depth, "limit": limit }))?;
        serde_json::from_value(v).map_err(|e| anyhow!("decode neighbors: {e}"))
    }
    pub fn recall_expanded(&self, query: &str, limit: usize, depth: usize) -> Result<Vec<Entry>> {
        let (mut seeds, neighbors) = self.recall_expanded_split(query, limit, depth)?;
        seeds.extend(neighbors);
        Ok(seeds)
    }
    pub fn recall_expanded_split(&self, query: &str, limit: usize, depth: usize) -> Result<(Vec<Entry>, Vec<Entry>)> {
        let g = self.recall_expanded_graph(query, limit, depth)?;
        Ok((g.seeds, g.riders.into_iter().map(|r| r.entry).collect()))
    }
    pub fn recall_expanded_graph(&self, query: &str, limit: usize, depth: usize) -> Result<RecallGraph> {
        let v = self.post("/recall_expanded", json!({ "query": query, "limit": limit, "depth": depth }))?;
        // Version-skew ladder, richest first: a current server returns {"seeds", "neighbors",
        // "riders", "links"}; a pre-provenance server omits riders/links (neighbors become
        // provenance-less riders: hop 1, empty via, score 0 - render falls back to the plain
        // `linked` marker); the oldest returns a flat array - treat that as all-seeds so skew
        // degrades to unmarked output, not an error.
        if v.is_array() {
            let seeds = serde_json::from_value(v).map_err(|e| anyhow!("decode /recall_expanded (flat): {e}"))?;
            return Ok(RecallGraph { seeds, riders: Vec::new(), links: Vec::new() });
        }
        let seeds: Vec<Entry> = serde_json::from_value(v.get("seeds").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| anyhow!("decode /recall_expanded seeds: {e}"))?;
        let riders: Vec<RecallRider> = match v.get("riders") {
            Some(r) => serde_json::from_value(r.clone()).map_err(|e| anyhow!("decode /recall_expanded riders: {e}"))?,
            None => {
                let neighbors: Vec<Entry> =
                    serde_json::from_value(v.get("neighbors").cloned().unwrap_or_else(|| json!([])))
                        .map_err(|e| anyhow!("decode /recall_expanded neighbors: {e}"))?;
                neighbors
                    .into_iter()
                    .map(|entry| RecallRider { entry, hop: 1, via: String::new(), rel: String::new(), score: 0.0 })
                    .collect()
            }
        };
        let links = serde_json::from_value(v.get("links").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| anyhow!("decode /recall_expanded links: {e}"))?;
        Ok(RecallGraph { seeds, riders, links })
    }
    pub fn reindex_links(&self) -> Result<(usize, usize)> {
        let v = self.post("/reindex_links", json!({}))?;
        // `pruned` is additive (new servers report it; old servers just don't).
        Ok((
            v.get("linked").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
            v.get("pruned").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        ))
    }
    pub fn reindex_mentions(&self, dry_run: bool) -> Result<(usize, usize)> {
        let v = self.post("/reindex_mentions", json!({ "dry_run": dry_run }))?;
        let found = v.get("found").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        let added = v.get("added").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        Ok((found, added))
    }
}

/// `dmem login`: write the `[server]` block into the config (preserving other keys), 0600. The
/// block is rebuilt from scratch, so a legacy `insecure` key does not survive a re-login.
pub fn login(url: &str, token: &str, ca_cert: Option<String>) -> Result<()> {
    let path = crate::config::config_path().ok_or_else(|| anyhow!("could not resolve a config dir"))?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut doc: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let mut server = toml::Table::new();
    server.insert("url".into(), toml::Value::String(url.trim_end_matches('/').to_string()));
    server.insert("token".into(), toml::Value::String(token.to_string()));
    if let Some(ca) = ca_cert {
        server.insert("ca_cert".into(), toml::Value::String(ca));
    }
    doc.insert("server".into(), toml::Value::Table(server));
    // 0600 from creation (the config holds the bearer token); atomic temp+rename, no chmod window.
    crate::config::write_secret(&path, &toml::to_string(&doc)?)?;
    println!("logged in to {url}\nconfig: {}", path.display());
    Ok(())
}

/// `dmem logout`: drop the `[server]` block, keeping any other config.
pub fn logout() -> Result<()> {
    let path = crate::config::config_path().ok_or_else(|| anyhow!("could not resolve a config dir"))?;
    if !path.exists() {
        println!("not logged in");
        return Ok(());
    }
    let mut doc: toml::Table = std::fs::read_to_string(&path)?.parse().unwrap_or_default();
    if doc.remove("server").is_none() {
        println!("not connected to a server");
        return Ok(());
    }
    std::fs::write(&path, toml::to_string(&doc)?)?;
    println!("logged out (server config removed)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, Kind};
    use serde_json::json;

    #[test]
    fn insecure_link_is_refused_with_the_fix_spelled_out() {
        let link = ServerLink { url: "https://x".into(), token: "t".into(), insecure: true, ca_cert: None };
        let err = RemoteClient::new(&link).err().expect("insecure must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("no longer honoured"), "{msg}");
        assert!(msg.contains("--ca-cert"), "must point at the pinning fix: {msg}");
    }

    #[test]
    fn verified_link_builds() {
        let link = ServerLink { url: "https://x/".into(), token: "t".into(), insecure: false, ca_cert: None };
        let c = RemoteClient::new(&link).expect("plain verified client");
        assert_eq!(c.base, "https://x", "trailing slash trimmed");
    }

    #[derive(Debug)]
    struct Nested(&'static str, Option<Box<Nested>>);
    impl std::fmt::Display for Nested {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for Nested {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|n| n as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn tls_trust_error_is_detected_anywhere_in_the_chain() {
        let deep = Nested("error sending request", Some(Box::new(Nested("client error (Connect)", Some(Box::new(Nested("invalid peer certificate: UnknownIssuer", None)))))));
        assert!(is_tls_trust_error(&deep));
        let plain = Nested("error sending request", Some(Box::new(Nested("connection refused", None))));
        assert!(!is_tls_trust_error(&plain));
    }

    /// The server returns a bare JSON array of Entry (e.g. `json!(m.persona()?)`); the client
    /// decodes it via `serde_json::from_value::<Vec<Entry>>`. This guards that contract so the
    /// remote persona/reminders path stays byte-compatible with the local one the feature relies on.
    #[test]
    fn server_entry_array_decodes_to_entries() {
        let e = Entry::new_now(
            "daimon://agent/persona/persona/op".into(),
            Kind::Persona,
            "agent/persona".into(),
            "Operator Persona".into(),
            "I am Izu.".into(),
            vec!["persona".into()],
            95,
            "daimon://agent/persona/persona/op".into(),
        );
        let wire = json!(vec![e.clone()]); // exactly what /persona serializes
        let back: Vec<Entry> = serde_json::from_value(wire).expect("server Entry array must decode");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].kind, Kind::Persona);
        assert_eq!(back[0].uri, e.uri);
        assert_eq!(back[0].tags, vec!["persona".to_string()]);
    }
}
