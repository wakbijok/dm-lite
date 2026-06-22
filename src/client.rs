//! Remote-client mode: talk to a remote `dmem serve` over HTTP(S) with a bearer token.
//! Selected when the config has a `[server]` block. Blocking reqwest (rustls), so the CLI and
//! hooks stay synchronous (no tokio). `insecure` accepts a self-signed cert; `ca_cert` trusts
//! a specific CA. The server enforces tenant isolation; this client just carries the token.

use crate::config::ServerLink;
use crate::entry::{Edge, Entry, Kind};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub struct RemoteClient {
    base: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl RemoteClient {
    pub fn new(link: &ServerLink) -> Result<Self> {
        let mut b = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(15));
        if link.insecure {
            b = b.danger_accept_invalid_certs(true);
        }
        if let Some(ca) = &link.ca_cert {
            let pem = std::fs::read(ca).map_err(|e| anyhow!("read ca_cert {ca}: {e}"))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| anyhow!("parse ca_cert: {e}"))?;
            b = b.add_root_certificate(cert);
        }
        let http = b.build().map_err(|e| anyhow!("build http client: {e}"))?;
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
            .map_err(|e| anyhow!("POST {path}: {e}"))?;
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
            .map_err(|e| anyhow!("GET {path}: {e}"))?;
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

    pub fn admin_add(&self, tenant: &str, label: &str, display: &str) -> Result<(String, String)> {
        let v = self.post("/admin/tenant", json!({ "tenant": tenant, "label": label, "display": display }))?;
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
        self.list("/recall_expanded", json!({ "query": query, "limit": limit, "depth": depth }))
    }
    pub fn reindex_links(&self) -> Result<usize> {
        let v = self.post("/reindex_links", json!({}))?;
        Ok(v.get("linked").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
    }
}

/// `dmem login`: write the `[server]` block into the config (preserving other keys), 0600.
pub fn login(url: &str, token: &str, insecure: bool, ca_cert: Option<String>) -> Result<()> {
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
    if insecure {
        server.insert("insecure".into(), toml::Value::Boolean(true));
    }
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
    use crate::entry::{Entry, Kind};
    use serde_json::json;

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
