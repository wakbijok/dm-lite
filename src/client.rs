//! Remote-client mode: talk to a remote `dmem serve` over HTTP(S) with a bearer token.
//! Selected when the config has a `[server]` block. Blocking reqwest (rustls), so the CLI and
//! hooks stay synchronous (no tokio). `insecure` accepts a self-signed cert; `ca_cert` trusts
//! a specific CA. The server enforces tenant isolation; this client just carries the token.

use crate::config::ServerLink;
use crate::entry::Entry;
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
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn list(&self, path: &str, body: Value) -> Result<Vec<Entry>> {
        let v = self.post(path, body)?;
        Ok(serde_json::from_value(v).unwrap_or_default())
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
    pub fn recall_as_of(&self, query: &str, limit: usize, as_of_ms: i64, _valid_ms: i64) -> Result<Vec<Entry>> {
        self.list("/recall", json!({ "query": query, "limit": limit, "as_of": as_of_ms }))
    }
    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        self.list("/recent", json!({ "limit": limit }))
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
    pub fn counts(&self) -> Result<Vec<(String, usize)>> {
        // counts live server-side; a remote `dmem status` reports the connection, not tallies.
        Ok(Vec::new())
    }
    pub fn recall_mode(&self) -> &'static str {
        "remote (HTTP client -> dmem serve)"
    }
    pub fn remember(&self, text: &str, namespace: &str) -> Result<String> {
        self.uri_of("/remember", json!({ "text": text, "namespace": namespace }))
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
}
