//! v1 -> v2 migration: import a daimon-memory (v1) export into dm-lite. The source is JSONL,
//! one record per line, from `GET /admin/export` (pulled here) or a file / stdin. Each record
//! maps to a dm-lite record, preserving the ORIGINAL creation time. Field names are read
//! defensively because the exact export shape is not pinned here.

use crate::entry::Kind;
use crate::tools::Memory;
use anyhow::{anyhow, Result};
use serde_json::Value;

/// daimon-memory `record_type` -> dm-lite Kind. dm-lite's kinds match v1 exactly, so this is a
/// 1:1 pass-through via Kind::from_str; a genuinely unknown kind falls back to Memory (never lost).
fn map_kind(s: &str) -> Kind {
    Kind::from_str(s).unwrap_or(Kind::Memory)
}

fn str_field<'a>(r: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| r.get(*k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()))
}

fn first_line(s: &str) -> String {
    s.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").chars().take(80).collect()
}

/// Days from civil date (Howard Hinnant), used to turn an RFC3339 stamp into epoch-ms with no
/// external date crate. Treats the time as UTC; ignores any fractional seconds / offset.
fn rfc3339_ms(s: &str) -> i64 {
    let num = |a: usize, n: usize| -> i64 { s.get(a..a + n).and_then(|x| x.parse::<i64>().ok()).unwrap_or(-1) };
    if s.len() < 19 {
        return 0;
    }
    let (y, mo, d) = (num(0, 4), num(5, 2), num(8, 2));
    let (h, mi, se) = (num(11, 2), num(14, 2), num(17, 2));
    if y < 1970 || mo < 1 || d < 1 {
        return 0;
    }
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    (((days * 24 + h) * 60 + mi) * 60 + se) * 1000
}

fn created_ms(r: &Value) -> i64 {
    for k in ["created_ms", "created_at_ms"] {
        if let Some(n) = r.get(k).and_then(|v| v.as_i64()) {
            return if n > 1_000_000_000_000 { n } else { n * 1000 };
        }
    }
    if let Some(s) = str_field(r, &["created_at", "created", "ts", "timestamp"]) {
        if let Ok(n) = s.parse::<i64>() {
            return if n > 1_000_000_000_000 { n } else { n * 1000 };
        }
        return rfc3339_ms(s);
    }
    0
}

/// namespace from an explicit field, else best-effort from a daimon:// uri (drop scheme + the
/// trailing <kind>/<id>; what remains is the namespace, possibly with a leading tenant).
fn namespace_of(r: &Value) -> String {
    if let Some(ns) = str_field(r, &["namespace", "ns"]) {
        return ns.to_string();
    }
    if let Some(uri) = str_field(r, &["uri", "id"]) {
        let body = uri.trim_start_matches("daimon://");
        let segs: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() > 2 {
            return segs[..segs.len() - 2].join("/");
        }
    }
    "resources/notes".to_string()
}

/// Original importance if the export carries one (0..=100), so migration preserves ranking
/// weight instead of resetting to the kind default.
fn importance_of(r: &Value) -> Option<i64> {
    r.get("importance").and_then(|v| v.as_i64()).filter(|n| (0..=100).contains(n))
}

/// Map one export record -> (kind, namespace, title, body, created_ms, importance). None if unusable.
fn map_record(r: &Value) -> Option<(Kind, String, String, String, i64, Option<i64>)> {
    let kind = map_kind(str_field(r, &["record_type", "kind", "type"]).unwrap_or("memory"));
    let ns = namespace_of(r);
    let body = str_field(r, &["body", "text", "content", "markdown", "abstract"])
        .map(|s| s.to_string())
        .unwrap_or_else(|| serde_json::to_string_pretty(r).unwrap_or_default());
    let title = str_field(r, &["title", "name", "summary"])
        .map(|s| s.to_string())
        .unwrap_or_else(|| first_line(&body));
    if title.trim().is_empty() {
        return None;
    }
    Some((kind, ns, title, body, created_ms(r), importance_of(r)))
}

/// The stderr line for a record the import skipped: its line number, its title (the first 60
/// characters) when it has one, and why.
fn skip_line(n: usize, title: Option<&str>, why: &str) -> String {
    match title {
        Some(t) => format!("skip line {n} ({}): {why}", t.chars().take(60).collect::<String>()),
        None => format!("skip line {n}: {why}"),
    }
}

/// Import every JSONL line into `m`. Returns (imported, skipped); each skipped record gets one
/// stderr line (`skip_line`), so a refusal (a scope guard, a server error) is not silent.
pub fn import_jsonl(m: &Memory, text: &str) -> (usize, usize) {
    let (mut ok, mut skip) = (0usize, 0usize);
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let r = match serde_json::from_str::<Value>(line) {
            Ok(r) => r,
            Err(e) => {
                skip += 1;
                eprintln!("{}", skip_line(i + 1, None, &format!("not JSON ({e})")));
                continue;
            }
        };
        match map_record(&r) {
            Some((kind, ns, title, body, created, importance)) => match m.import_record_at(kind, &ns, &title, &body, created, importance) {
                Ok(_) => ok += 1,
                Err(e) => {
                    skip += 1;
                    eprintln!("{}", skip_line(i + 1, Some(&title), &format!("{e:#}")));
                }
            },
            None => {
                skip += 1;
                eprintln!("{}", skip_line(i + 1, None, "no title"));
            }
        }
    }
    (ok, skip)
}

/// Pull a JSONL export from a running daimon-memory v1 (`GET /admin/export`). TLS is always
/// verified (the request carries the v1 admin token, so accepting any cert would expose it to a
/// MITM); `ca_cert` trusts a specific PEM for a self-signed v1 server.
fn fetch_export(url: &str, token: &str, ca_cert: Option<&str>) -> Result<String> {
    let base = url.trim_end_matches('/');
    let mut builder = crate::tls::http_builder().timeout(std::time::Duration::from_secs(180));
    if let Some(ca) = ca_cert {
        let pem = std::fs::read(ca).map_err(|e| anyhow!("read ca_cert {ca}: {e}"))?;
        let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| anyhow!("parse ca_cert {ca}: {e}"))?;
        builder = builder.add_root_certificate(cert);
    }
    let http = builder.build()?;
    let resp = http
        .get(format!("{base}/admin/export"))
        .bearer_auth(token)
        .send()
        // keep the source chain: reqwest's own message never names the TLS cause, `main`'s `{:#}` does
        .map_err(|e| anyhow::Error::new(e).context(format!("GET {base}/admin/export")))?;
    if !resp.status().is_success() {
        anyhow::bail!("server {} on /admin/export: {}", resp.status().as_u16(), resp.text().unwrap_or_default().trim());
    }
    Ok(resp.text().unwrap_or_default())
}

pub fn run(file: Option<String>, url: Option<String>, token: Option<String>, ca_cert: Option<String>) -> Result<()> {
    let text = if let Some(u) = url {
        let t = token.ok_or_else(|| anyhow!("--url needs --token (the v1 admin token)"))?;
        fetch_export(&u, &t, ca_cert.as_deref())?
    } else if let Some(f) = file {
        if f == "-" {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        } else {
            std::fs::read_to_string(&f)?
        }
    } else {
        return Err(anyhow!("migrate needs --file <jsonl> (or - for stdin), or --url <v1> --token <t>"));
    };
    let m = Memory::open()?;
    let (ok, skip) = import_jsonl(&m, &text);
    println!("migrated {ok} records, skipped {skip}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_v1_record_with_timestamp() {
        let r: Value = serde_json::from_str(
            r#"{"record_type":"agent_lesson","namespace":"resources/homelab",
                "title":"Ceph re-adoption","body":"Rook does not auto re-adopt an OSD.",
                "created_at":"2026-06-14T09:30:00Z","importance":75}"#,
        )
        .unwrap();
        let (kind, ns, title, body, created, importance) = map_record(&r).unwrap();
        assert_eq!(kind, Kind::AgentLesson); // agent_lesson -> AgentLesson (1:1 with v1)
        assert_eq!(ns, "resources/homelab");
        assert_eq!(title, "Ceph re-adoption");
        assert!(body.contains("auto re-adopt"));
        assert!(created > 1_780_000_000_000, "created_at parsed to epoch-ms: {created}");
        assert_eq!(importance, Some(75)); // original importance preserved, not reset to kind default
    }

    #[test]
    fn v1_kinds_pass_through_and_namespace_from_uri() {
        // a v1 kind dm-lite now carries 1:1
        let r: Value = serde_json::from_str(
            r#"{"record_type":"known_failure_mode","uri":"daimon://resources/project-x/known_failure_mode/abc",
                "text":"re-running the nightly export job overwrites reports already signed off"}"#,
        )
        .unwrap();
        let (kind, ns, title, _b, _c, importance) = map_record(&r).unwrap();
        assert_eq!(kind, Kind::KnownFailureMode); // 1:1, no longer flattened to Memory
        assert_eq!(ns, "resources/project-x");
        assert!(title.starts_with("re-running the nightly export")); // title inferred from body
        assert_eq!(importance, None); // no importance field -> fall back to kind default at write time

        // a genuinely unknown future kind still falls back to Memory, never dropped
        let r2: Value = serde_json::from_str(r#"{"record_type":"some_future_kind","title":"x","body":"y"}"#).unwrap();
        assert_eq!(map_record(&r2).unwrap().0, Kind::Memory);
    }

    // A skipped record is counted and named on stderr: its line number, its title (the first
    // 60 characters) and why. Before, a refused record was only counted.
    #[test]
    fn skipped_records_are_counted_and_named() {
        assert_eq!(skip_line(3, None, "no title"), "skip line 3: no title");
        let long = "t".repeat(80);
        assert_eq!(skip_line(7, Some(&long), "why"), format!("skip line 7 ({}): why", "t".repeat(60)));

        let dir = std::env::temp_dir().join(format!("dmmigrate-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut l = crate::tools::LocalMemory::for_test(crate::sqlite::SqliteStore::open(&dir.join("t.db")).unwrap());
        // a scope-bound identity: its backdated record is refused, the others import
        l.set_scope_context(Some("room:a"), Some(vec!["room:a".into()]));
        let m = Memory::Local(l);
        let text = [
            r#"{"title":"kept","body":"b"}"#,
            "",
            "not json",
            r#"{"title":"old","body":"b","created_ms":1600000000000}"#,
            r#"{"title":"   ","body":"b"}"#,
        ]
        .join("\n");
        assert_eq!(import_jsonl(&m, &text), (1, 3));
    }

    // The v1 export pull offers the post-quantum hybrid first, as every dmem TLS client does.
    #[test]
    fn export_client_offers_the_pq_hybrid_first() {
        let groups = crate::tls::tests::offered_groups(|url| {
            let _ = fetch_export(&url, "t", None);
        });
        assert_eq!(groups.first(), Some(&rustls::NamedGroup::X25519MLKEM768), "{groups:?}");
    }

    // A pinned certificate is judged by rustls, stricter than the OpenSSL a 0.3.7 release used: a
    // self-signed server certificate that is also a CA (what a plain `openssl req -x509` makes)
    // is refused, and the error says why; one that is not a CA, as `--tls-generate` writes, passes.
    #[cfg(feature = "server")]
    #[test]
    fn export_pull_names_why_a_pinned_cert_is_refused() {
        use std::io::{Read, Write};
        let pull = |is_ca: bool| {
            let key = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
            if is_ca {
                params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            }
            let cert = params.self_signed(&key).unwrap();
            let pem = std::env::temp_dir().join(format!("dmpin-{}-{}-{is_ca}.pem", std::process::id(), crate::entry::now_ms()));
            std::fs::write(&pem, cert.pem()).unwrap();
            crate::tls::install_provider();
            let key = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
            let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert.der().clone()], key.into()).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("https://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let server = std::thread::spawn(move || {
                let (sock, _) = listener.accept().unwrap();
                let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap(), sock);
                // a refused certificate ends the handshake inside the first read
                let (mut req, mut buf) = (Vec::new(), [0u8; 1024]);
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
            let out = fetch_export(&url, "t", Some(pem.to_str().unwrap()));
            server.join().unwrap();
            let _ = std::fs::remove_file(&pem);
            format!("{:#}", out.expect_err("no export behind this server"))
        };
        let err = pull(true);
        assert!(err.contains("CaUsedAsEndEntity"), "the cause must reach the user: {err}");
        let err = pull(false);
        assert!(err.contains("server 404 on /admin/export"), "past TLS to the server: {err}");
    }
}
