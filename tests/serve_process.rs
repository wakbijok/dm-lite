//! Process-level tests of `dmem serve`: the real binary, started the way a service manager
//! starts it, stopped with a real signal. What the in-process router tests cannot see: the
//! process umask, what the server prints and writes at start-up, and how it exits.
#![cfg(all(unix, feature = "server"))]
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static N: AtomicU64 = AtomicU64::new(0);

/// A fresh directory for one test (removed by `Serve::drop` unless the test failed).
fn scratch(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("dmproc-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub const TENANT_TOKEN: &str = "tenant-secret-0123456789abcdef";

/// An `env` value that removes the variable from the child's environment instead of setting it
/// (no environment value can hold a NUL byte).
pub const UNSET: &str = "\0";

/// Apply `env` to `cmd`, removing each variable whose value is `UNSET`.
fn apply_env(cmd: &mut Command, env: &[(&str, &str)]) {
    for (k, v) in env {
        if *v == UNSET {
            cmd.env_remove(k);
        } else {
            cmd.env(k, v);
        }
    }
}

/// One `dmem serve` child with its data dir, address and captured stderr.
pub struct Serve {
    pub child: Child,
    pub data: PathBuf,
    pub addr: String,
    pub log: Arc<Mutex<String>>,
    root: PathBuf,
}

impl Serve {
    /// Start `dmem serve` on a free loopback port over a fresh data dir under `root`, with one
    /// env tenant token (`DM_TOKEN_T`) plus `env`, and `extra` arguments. `DM_REQUIRE_MODEL` is
    /// unset, so the model is required as in production: on a host where it neither is cached
    /// nor can download, the server refuses to start and says why. A test that runs a server on
    /// the placeholder sets `DM_REQUIRE_MODEL=0` in `env`.
    pub fn start(root: &Path, env: &[(&str, &str)], extra: &[&str]) -> Serve {
        let data = root.join("data");
        Self::start_on(root, &data, env, extra)
    }

    pub fn start_on(root: &Path, data: &Path, env: &[(&str, &str)], extra: &[&str]) -> Serve {
        let addr = format!("127.0.0.1:{}", free_port());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dmem"));
        cmd.args(["serve", "--addr", &addr])
            .args(extra)
            .env("DM_DATA_DIR", data)
            // never the developer's own config (a [server] block, a data_dir)
            .env("DM_CONFIG", root.join("no-config.toml"))
            .env("DM_TOKEN_T", TENANT_TOKEN)
            .env_remove("DM_ADMIN_TOKEN")
            .env_remove("DM_REQUIRE_MODEL")
            .env_remove("DM_REQUIRE_VECTORS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        apply_env(&mut cmd, env);
        let mut child = cmd.spawn().expect("spawn dmem serve");
        let log = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut l = sink.lock().unwrap();
                l.push_str(&line);
                l.push('\n');
            }
        });
        Serve { child, data: data.to_path_buf(), addr, log, root: root.to_path_buf() }
    }

    pub fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// Wait until the log shows `needle` (or the process exits, or `secs` pass).
    pub fn wait_log(&mut self, needle: &str, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.log().contains(needle) {
                return true;
            }
            if let Ok(Some(_)) = self.child.try_wait() {
                std::thread::sleep(Duration::from_millis(100)); // let the reader drain
                return self.log().contains(needle);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Wait until plain-HTTP `/healthz` answers 200.
    pub fn wait_healthy(&mut self, secs: u64) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok((200, _)) = http(&self.addr, "GET", "/healthz", None, None) {
                return;
            }
            if let Ok(Some(st)) = self.child.try_wait() {
                panic!("dmem serve exited early ({st}):\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("dmem serve never became healthy:\n{}", self.log());
    }

    pub fn signal(&self, sig: i32) {
        // SAFETY: kill(2) on our own child's pid.
        let rc = unsafe { libc::kill(self.child.id() as i32, sig) };
        assert_eq!(rc, 0, "kill failed");
    }

    /// Stop the server outright (SIGKILL) and wait for it.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(100)); // let the reader drain
    }

    pub fn wait_exit(&mut self, secs: u64) -> ExitStatus {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                std::thread::sleep(Duration::from_millis(100)); // let the reader drain
                return st;
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                panic!("dmem serve did not exit within {secs}s:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// One plain HTTP/1.1 request (`Connection: close`): (status, body).
pub fn http(addr: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> std::io::Result<(u16, String)> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status line in {text:?}")))?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Ok((status, body))
}

/// `GET /readyz` once the start-up pass is over (it opens every known tenant before the server
/// reports ready): (status, body).
pub fn readyz_settled(addr: &str) -> (u16, String) {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, body) = http(addr, "GET", "/readyz", None, None).unwrap();
        if !body.contains(r#""startup":"fail""#) || Instant::now() > end {
            return (st, body);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Every regular file under `dir`, with its permission bits.
fn modes_under(dir: &Path) -> Vec<(PathBuf, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let md = e.metadata().unwrap();
            if md.is_dir() {
                out.push((e.path(), md.permissions().mode() & 0o777));
                stack.push(e.path());
            } else {
                out.push((e.path(), md.permissions().mode() & 0o777));
            }
        }
    }
    out
}

// P0-6: `dmem serve` runs with umask 077, so everything it creates in the data dir (the tenant
// database and its -wal/-shm, iam.db and its -wal/-shm, the vector index, the TLS key) is
// owner-only whatever the umask it was started with. The public TLS certificate is the one
// deliberate exception.
#[test]
fn serve_creates_only_owner_only_files() {
    let root = scratch("umask");
    // SAFETY: umask(2); a permissive mask for the child to inherit (restored below).
    let before = unsafe { libc::umask(0o002) };
    let mut s = Serve::start(&root, &[], &["--tls-generate"]);
    unsafe { libc::umask(before) };
    assert!(s.wait_log("listening on https://", 300), "{}", s.log());
    s.kill();
    let files = modes_under(&s.data);
    assert!(files.iter().any(|(p, _)| p.ends_with("iam.db")), "{files:?}");
    assert!(files.iter().any(|(p, _)| p.ends_with("tls/key.pem")), "{files:?}");
    for (p, mode) in &files {
        if p.ends_with("tls/cert.pem") {
            assert_eq!(*mode, 0o644, "the public certificate stays readable");
            continue;
        }
        assert_eq!(mode & 0o077, 0, "{} is {mode:03o}, not owner-only", p.display());
    }
}

// P0-6, over plain HTTP with a tenant write: the tenant database, its -wal/-shm and the vector
// index are owner-only too.
#[test]
fn tenant_files_are_owner_only() {
    let root = scratch("umask2");
    let before = unsafe { libc::umask(0o002) };
    let mut s = Serve::start(&root, &[], &[]);
    unsafe { libc::umask(before) };
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "POST", "/remember", Some(TENANT_TOKEN), Some(r#"{"text":"umask probe note"}"#)).unwrap();
    assert_eq!(st, 200, "{body}");
    let files = modes_under(&s.data);
    assert!(files.iter().any(|(p, _)| p.ends_with("tenants/t.db-wal")), "{files:?}");
    for (p, mode) in &files {
        assert_eq!(mode & 0o077, 0, "{} is {mode:03o}, not owner-only", p.display());
    }
    s.kill();
}

// P0-6: with DM_ADMIN_TOKEN set, no bootstrap admin token is printed or written, and the env
// token is the admin token. Without it, a fresh data dir mints, prints and writes one (so this
// test would see a leak).
#[test]
fn admin_token_from_env_is_never_printed_or_written() {
    let admin = "env-admin-token-0123456789abcdef";
    let root = scratch("admintok");
    let mut s = Serve::start(&root, &[("DM_ADMIN_TOKEN", admin)], &[]);
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "GET", "/admin/tenants", Some(admin), None).unwrap();
    assert_eq!(st, 200, "the env token is the admin token: {body}");
    s.kill();
    let log = s.log();
    assert!(!log.contains("ROOT ADMIN"), "{log}");
    assert!(!log.contains("dmem_"), "no minted token text in the log: {log}");
    assert!(!s.data.join("admin.token").exists());

    // set but empty: still nothing minted, printed or written
    let root = scratch("admintok-empty");
    let mut s = Serve::start(&root, &[("DM_ADMIN_TOKEN", "")], &[]);
    s.wait_healthy(300);
    s.kill();
    assert!(!s.log().contains("ROOT ADMIN"), "{}", s.log());
    assert!(s.log().contains("DM_ADMIN_TOKEN is set but empty"), "{}", s.log());
    assert!(!s.data.join("admin.token").exists());

    // control: unset on a fresh data dir mints, prints and writes the bootstrap token
    let root = scratch("admintok-ctl");
    let mut s = Serve::start(&root, &[], &[]);
    s.wait_healthy(300);
    s.kill();
    assert!(s.log().contains("generated ROOT ADMIN token"), "{}", s.log());
    assert!(s.data.join("admin.token").exists());
}

/// Environment in which the embedding model cannot load: an empty home (no model cache) and
/// every proxy pointed at a closed local port (no download), so the loaders fall back to the
/// placeholder hash embedder, as an air-gapped host without the cache does.
fn model_unavailable_env(root: &Path) -> Vec<(&'static str, String)> {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let dead = "http://127.0.0.1:9".to_string();
    vec![
        ("HOME", home.display().to_string()),
        ("HF_HOME", home.join("hf").display().to_string()),
        ("HUGGINGFACE_HUB_CACHE", home.join("hf/hub").display().to_string()),
        ("HTTPS_PROXY", dead.clone()),
        ("HTTP_PROXY", dead.clone()),
        ("ALL_PROXY", dead.clone()),
        ("https_proxy", dead.clone()),
        ("http_proxy", dead.clone()),
        ("all_proxy", dead),
        ("NO_PROXY", String::new()),
        ("no_proxy", String::new()),
    ]
}

fn as_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    v.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

// P0-4: DM_REQUIRE_MODEL=1 (like the default, unset) refuses to start when the embedding model
// did not load (the server would otherwise run on the placeholder hash embedder and silently
// degrade every recall). Control: the same host opted out (DM_REQUIRE_MODEL=0) starts, on the
// placeholder, and says so.
#[cfg(feature = "zvec")]
#[test]
fn require_model_refuses_to_start_on_the_placeholder_embedder() {
    let root = scratch("reqmodel");
    let env = model_unavailable_env(&root);
    let mut with_flag = as_refs(&env);
    with_flag.push(("DM_REQUIRE_MODEL", "1"));
    let mut s = Serve::start(&root, &with_flag, &[]);
    let st = s.wait_exit(300);
    assert!(!st.success(), "must not start: {}", s.log());
    assert!(s.log().contains("DM_REQUIRE_MODEL is set"), "{}", s.log());
    assert!(s.log().contains("refusing to start"), "{}", s.log());
    assert!(http(&s.addr, "GET", "/healthz", None, None).is_err(), "nothing listens");

    let root = scratch("reqmodel-ctl");
    let env = model_unavailable_env(&root);
    let admin = "env-admin-token-0123456789abcdef";
    let mut opted_out = as_refs(&env);
    opted_out.extend([("DM_REQUIRE_MODEL", "0"), ("DM_ADMIN_TOKEN", admin)]);
    let mut s = Serve::start(&root, &opted_out, &[]);
    s.wait_healthy(300);
    assert!(s.log().contains("placeholder hash embedder is active"), "{}", s.log());
    // one loud line, once
    assert_eq!(s.log().matches("WARNING memory recall is degraded").count(), 1, "{}", s.log());
    // P0-3: live but not ready, and /readyz says which check fails
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains(r#""failing":["embedder"]"#), "{body}");
    assert_eq!(http(&s.addr, "GET", "/healthz", None, None).unwrap().0, 200);
    let (_, body) = http(&s.addr, "GET", "/readyz", Some(admin), None).unwrap();
    assert!(body.contains("degraded: placeholder embedder"), "the admin token sees why: {body}");
    s.kill();
}

// P0-4: with DM_REQUIRE_VECTORS a tenant whose vector index cannot open fails its requests
// instead of silently answering keyword-only; without it, the old keyword-only fallback.
#[cfg(feature = "zvec")]
#[test]
fn require_vectors_fails_a_tenant_whose_vector_index_cannot_open() {
    let shared = scratch("reqvec-data");
    let data = shared.join("data");
    std::fs::create_dir_all(data.join("vectors")).unwrap();
    std::fs::write(data.join("vectors").join("t"), "not a vector index").unwrap(); // breaks the open

    let root = scratch("reqvec-off");
    let mut s = Serve::start_on(&root, &data, &[], &[]);
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "POST", "/remember", Some(TENANT_TOKEN), Some(r#"{"text":"zrv keyword only note"}"#)).unwrap();
    assert_eq!(st, 200, "{body}");
    let (st, body) = http(&s.addr, "POST", "/recall", Some(TENANT_TOKEN), Some(r#"{"query":"zrv keyword"}"#)).unwrap();
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("zrv keyword only note"), "keyword-only recall still answers: {body}");
    assert!(s.log().contains("falling back to keyword-only recall"), "{}", s.log());
    // P0-3: the degraded tenant makes the server not ready (without naming it in the body)
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains(r#""failing":["vectors"]"#), "{body}");
    assert!(s.wait_log("not ready: vectors", 10), "{}", s.log());
    s.kill();

    let root = scratch("reqvec-on");
    let mut s = Serve::start_on(&root, &data, &[("DM_REQUIRE_VECTORS", "1")], &[]);
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "POST", "/recall", Some(TENANT_TOKEN), Some(r#"{"query":"zrv keyword"}"#)).unwrap();
    assert_eq!(st, 500, "the tenant fails instead of degrading: {body}");
    assert!(!body.contains("zrv"), "{body}");
    let (st, _) = http(&s.addr, "POST", "/remember", Some(TENANT_TOKEN), Some(r#"{"text":"zrv not stored"}"#)).unwrap();
    assert_eq!(st, 500);
    assert!(s.log().contains("DM_REQUIRE_VECTORS is set"), "{}", s.log());
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 503, "a refused tenant counts as one without a vector index: {body}");
    assert!(body.contains(r#""failing":["vectors"]"#), "{body}");
    s.kill();
    let _ = std::fs::remove_dir_all(&shared);
}

/// Wait (up to `secs`) until `/readyz` no longer lists `check` as failing: (status, body).
fn wait_check_passes(addr: &str, check: &str, secs: u64) -> (u16, String) {
    let end = Instant::now() + Duration::from_secs(secs);
    let failing = format!(r#""{check}":"fail""#);
    loop {
        let (st, body) = http(addr, "GET", "/readyz", None, None).unwrap();
        if !body.contains(&failing) || Instant::now() > end {
            return (st, body);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// A tenant whose vector index could not open no longer keeps the server not ready until a
// restart: the index is retried on a schedule (first after 5 s), readiness comes back once
// the cause is gone, with no request to the tenant, and the note saved meanwhile gets its
// vector. A restart over the same data dir judges the tenant at start-up, before any request.
#[cfg(feature = "zvec")]
#[test]
fn a_vector_index_that_could_not_open_heals_without_a_restart() {
    let shared = scratch("vecheal-data");
    let data = shared.join("data");
    std::fs::create_dir_all(data.join("vectors")).unwrap();
    let root = scratch("vecheal");
    let blocker = data.join("vectors").join("t");
    std::fs::write(&blocker, "not a vector index").unwrap();
    let mut s = Serve::start_on(&root, &data, &[], &[]);
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "POST", "/remember", Some(TENANT_TOKEN), Some(r#"{"text":"zvh saved while keyword only"}"#)).unwrap();
    assert_eq!(st, 200, "{body}");
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains(r#""vectors":"fail""#), "{body}");
    std::fs::remove_file(&blocker).unwrap();
    let (_, body) = wait_check_passes(&s.addr, "vectors", 60);
    assert!(!body.contains(r#""vectors":"fail""#), "never healed: {body}\n{}", s.log());
    assert!(s.wait_log("vector index opened on retry", 10), "{}", s.log());
    assert!(s.wait_log("re-embedded 1 record(s)", 30), "{}", s.log());
    s.signal(libc::SIGTERM);
    assert!(s.wait_exit(60).success(), "{}", s.log());
    assert_eq!(uris_in_vector_index(&data, "t").len(), 1, "the keyword-only save has its vector now");

    // restart with the index blocked again: the start-up pass opens the tenant (it has a
    // database) and readiness fails before any request reaches it
    std::fs::remove_dir_all(data.join("vectors").join("t")).unwrap();
    std::fs::write(&blocker, "not a vector index").unwrap();
    let root2 = scratch("vecheal-restart");
    let mut s = Serve::start_on(&root2, &data, &[], &[]);
    s.wait_healthy(300);
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains(r#""vectors":"fail""#), "judged at start-up: {body}\n{}", s.log());
    assert!(s.log().contains("start-up checks done (1 known tenant(s) opened)"), "{}", s.log());
    s.kill();
    let _ = std::fs::remove_dir_all(&shared);
}

/// Wait (up to 10 s) until a fresh connection to `addr` is refused (the listener is closed).
fn wait_refused(addr: &str) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if TcpStream::connect(addr).is_err() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Uris with a vector in the tenant's index on disk (read after the server exited).
#[cfg(feature = "zvec")]
fn uris_in_vector_index(data: &Path, tenant: &str) -> Vec<String> {
    let idx = dm_lite::zvec_index::ZvecIndex::open(&data.join("vectors").join(tenant)).unwrap();
    let probe = vec![1.0 / (dm_lite::embedder::DIM as f32).sqrt(); dm_lite::embedder::DIM];
    let mut uris: Vec<String> = idx.search(&probe, 50).unwrap().into_iter().map(|h| h.uri).collect();
    uris.sort();
    uris
}

/// Live bodies in the tenant's database on disk.
fn live_bodies(data: &Path, tenant: &str) -> Vec<String> {
    let c = rusqlite::Connection::open(data.join("tenants").join(format!("{tenant}.db"))).unwrap();
    let mut st = c.prepare("SELECT body FROM entries WHERE system_to_ms IS NULL ORDER BY body").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
}

// P0-2: SIGTERM on the plain path. The server stops accepting at once, lets the request already
// in flight finish (here: its body is still arriving when the signal lands), flushes the vector
// index, closes the tenant and exits 0. Both saves are on disk, record and vector.
#[test]
fn sigterm_drains_the_request_in_flight_then_flushes_and_exits_cleanly() {
    let root = scratch("sigterm");
    let mut s = Serve::start(&root, &[], &[]);
    s.wait_healthy(300);
    let (st, body) = http(&s.addr, "POST", "/remember", Some(TENANT_TOKEN), Some(r#"{"text":"zst saved before the stop"}"#)).unwrap();
    assert_eq!(st, 200, "{body}");
    // P0-3: a healthy server with its model loaded is ready (the suite needs the model cached,
    // as the embedder tests do; on the placeholder /readyz is rightly 503)
    if !s.log().contains("placeholder hash embedder is active") {
        let (st, body) = readyz_settled(&s.addr);
        assert_eq!(st, 200, "{body}");
        assert!(body.contains(r#""status":"ready""#), "{body}");
    }

    // a save whose body is only half sent when SIGTERM arrives
    let payload = r#"{"text":"zst saved while draining"}"#;
    let (head, tail) = payload.split_at(12);
    let mut c = TcpStream::connect(&s.addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
    write!(
        c,
        "POST /remember HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {TENANT_TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{head}",
        payload.len()
    )
    .unwrap();
    c.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    s.signal(libc::SIGTERM);
    assert!(s.wait_log("SIGTERM received", 30), "{}", s.log());
    assert!(s.log().contains("draining 1 in-flight request(s)"), "{}", s.log());
    assert!(wait_refused(&s.addr), "still accepting connections after SIGTERM:\n{}", s.log());
    c.write_all(tail.as_bytes()).unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "the in-flight save completed: {resp}");

    let st = s.wait_exit(120);
    assert!(st.success(), "exit {st}:\n{}", s.log());
    let log = s.log();
    assert!(log.contains("closed 1 tenant(s)"), "{log}");
    #[cfg(feature = "zvec")]
    assert!(log.contains("vector index flushed for 1"), "{log}");
    assert!(log.contains("shutdown complete"), "{log}");
    assert_eq!(live_bodies(&s.data, "t"), vec!["zst saved before the stop", "zst saved while draining"]);
    #[cfg(feature = "zvec")]
    assert_eq!(uris_in_vector_index(&s.data, "t").len(), 2, "both vectors are on disk");
}

// P0-2: the TLS path had no stop handling at all (a SIGTERM killed it mid-request); SIGINT
// (ctrl-c) on either path stops it the same way.
#[test]
fn sigterm_on_tls_and_sigint_on_plain_exit_cleanly() {
    let root = scratch("sigterm-tls");
    let mut s = Serve::start(&root, &[], &["--tls-generate"]);
    assert!(s.wait_log("listening on https://", 300), "{}", s.log());
    s.signal(libc::SIGTERM);
    let st = s.wait_exit(60);
    assert!(st.success(), "exit {st}:\n{}", s.log());
    assert!(s.log().contains("SIGTERM received") && s.log().contains("shutdown complete"), "{}", s.log());

    let root = scratch("sigint");
    let mut s = Serve::start(&root, &[], &[]);
    s.wait_healthy(300);
    s.signal(libc::SIGINT);
    let st = s.wait_exit(60);
    assert!(st.success(), "exit {st}:\n{}", s.log());
    assert!(s.log().contains("SIGINT received") && s.log().contains("shutdown complete"), "{}", s.log());
}

/// Run one `dmem` command to completion, `stdin` fed then closed: (status, stdout, stderr). No
/// config and a data dir under `root`, none of the caller's server, tenant, scope, model or
/// cache switches; then `env`.
fn run_dmem(root: &Path, env: &[(&str, &str)], args: &[&str], stdin: &str) -> (ExitStatus, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dmem"));
    cmd.args(args)
        .env("DM_DATA_DIR", root.join("data"))
        .env("DM_CONFIG", root.join("no-config.toml"))
        .env("DM_CLAUDE_DIR", root.join("claude"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in [
        "DM_ENDPOINT", "DM_TOKEN", "DM_TENANT", "DM_SCOPE", "DM_WRITE_SCOPE", "DM_ADMIN_TOKEN", "DM_REQUIRE_MODEL",
        "DM_REQUIRE_VECTORS", "DM_CANDLE_MODEL", "HF_HOME", "HUGGINGFACE_HUB_CACHE", "HF_HUB_OFFLINE",
    ] {
        cmd.env_remove(k);
    }
    apply_env(&mut cmd, env);
    let mut child = cmd.spawn().expect("spawn dmem");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status, String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// One MCP request line: a `ping`.
const MCP_PING: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";

/// A stand-in for the model host: every proxy variable points at it, so a model download
/// connects here (and is closed at once) instead of reaching the network. Counts the attempts.
struct Canary {
    url: String,
    hits: Arc<AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Canary {
    fn start() -> Canary {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let (hits, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let (h, st) = (hits.clone(), stop.clone());
        std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                match l.accept() {
                    // dropped: the client sees the connection close before any answer
                    Ok(_) => {
                        h.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Canary { url, hits, stop }
    }

    /// Every proxy variable at the canary. Loopback is exempt for the remote client (reqwest
    /// honours NO_PROXY); the model loader (hf-hub's ureq) ignores NO_PROXY, so a model download
    /// still lands here. A download blocks until the canary closes it, so once the command has
    /// exited, `hits` counts every attempt it made.
    fn env(&self) -> Vec<(&'static str, String)> {
        let mut v: Vec<(&'static str, String)> = ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
            .iter()
            .map(|k| (*k, self.url.clone()))
            .collect();
        v.extend([("NO_PROXY", "127.0.0.1,localhost".to_string()), ("no_proxy", "127.0.0.1,localhost".to_string())]);
        v
    }

    fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A host without the model: an empty home (the default cache is under it), no cache
/// variables, the model requirement at its default (unset), and every proxy at `canary`.
fn no_model_env(root: &Path, canary: &Canary) -> Vec<(&'static str, String)> {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut v = vec![
        ("HOME", home.display().to_string()),
        ("HF_HOME", UNSET.to_string()),
        ("HUGGINGFACE_HUB_CACHE", UNSET.to_string()),
        ("HF_HUB_OFFLINE", UNSET.to_string()),
        ("DM_CANDLE_MODEL", UNSET.to_string()),
        ("DM_REQUIRE_MODEL", UNSET.to_string()),
    ];
    v.extend(canary.env());
    v
}

/// A model cache at `hub` holding the model this test process has cached (as the embedder tests
/// need it), as symlinks to its files: the real cache is only read.
#[cfg(feature = "candle")]
fn mirror_model_cache(hub: &Path) {
    let src_hub = dm_lite::embedder::hf_hub_cache_dir().expect("a model cache dir");
    let src = src_hub.join("models--BAAI--bge-small-en-v1.5");
    let rev = std::fs::read_to_string(src.join("refs").join("main")).unwrap_or_else(|e| {
        panic!("this test needs BAAI/bge-small-en-v1.5 in the model cache {} (as the embedder tests do): {e}", src_hub.display())
    });
    let repo = hub.join("models--BAAI--bge-small-en-v1.5");
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    std::fs::write(repo.join("refs").join("main"), &rev).unwrap();
    let dst = repo.join("snapshots").join(&rev);
    std::fs::create_dir_all(&dst).unwrap();
    for f in ["config.json", "tokenizer.json", "model.safetensors"] {
        let file = std::fs::canonicalize(src.join("snapshots").join(&rev).join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        std::os::unix::fs::symlink(file, dst.join(f)).unwrap();
    }
}

// The embedding model is required by default. With no model in the cache and HF_HUB_OFFLINE=1,
// `dmem serve` does not start and a local tenant does not open (CLI and MCP), each with one error
// naming the model, the cache it looked in, how to pre-install the model and the escape hatch,
// and nothing reaches the network. Control: without HF_HUB_OFFLINE the loader does try the
// network (the canary sees it), so the silence is the offline switch at work.
#[cfg(feature = "candle")]
#[test]
fn the_model_is_required_by_default_and_offline_loads_from_the_cache_only() {
    let canary = Canary::start();
    let root = scratch("modeldefault");
    let mut env = no_model_env(&root, &canary);
    env.push(("HF_HUB_OFFLINE", "1".to_string()));
    let env = as_refs(&env);
    let cache = root.join("home/.cache/huggingface/hub").display().to_string();
    let explains = |log: &str| {
        for needle in [
            "the embedding model BAAI/bge-small-en-v1.5 did not load",
            cache.as_str(),
            "the model is not there",
            "HF_HUB_OFFLINE forbids a download",
            "`dmem doctor`",
            "DM_REQUIRE_MODEL is set to 0",
        ] {
            assert!(log.contains(needle), "{needle:?} missing:\n{log}");
        }
    };

    let mut s = Serve::start(&root, &env, &[]);
    let st = s.wait_exit(300);
    assert!(!st.success(), "must not start: {}", s.log());
    assert!(s.log().contains("refusing to start"), "{}", s.log());
    explains(&s.log());
    assert!(http(&s.addr, "GET", "/healthz", None, None).is_err(), "nothing listens");
    assert!(!s.data.join("iam.db").exists(), "refused before IAM mints anything");

    // the embedded CLI: the local tenant does not open, and nothing is written
    let (st, out, err) = run_dmem(&root, &env, &["remember", "zmr refused note"], "");
    assert!(!st.success(), "{out}{err}");
    explains(&err);
    assert!(!root.join("data/tenants/default.db").exists());
    // MCP over stdio: no server comes up
    let (st, out, err) = run_dmem(&root, &env, &["mcp"], MCP_PING);
    assert!(!st.success(), "{out}{err}");
    assert!(out.is_empty(), "no MCP answer: {out}");
    explains(&err);
    // doctor tells the same up front, loading nothing
    let (st, out, err) = run_dmem(&root, &env, &["doctor", "--json"], "");
    assert!(st.success(), "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["model_cache"], cache.as_str(), "{v}");
    assert_eq!(
        (v["model_cached"].as_bool(), v["model_required"].as_bool(), v["hub_offline"].as_bool()),
        (Some(false), Some(true), Some(true)),
        "{v}"
    );
    assert!(v["model_state"].as_str().unwrap().starts_with("REFUSES to run"), "{v}");
    assert_eq!(canary.hits(), 0, "HF_HUB_OFFLINE=1 never reaches the network");

    // control: online, the same host tries to download the model, and still refuses
    let online = no_model_env(&root, &canary);
    let (st, out, err) = run_dmem(&root, &as_refs(&online), &["remember", "zmr refused note"], "");
    assert!(!st.success(), "{out}{err}");
    assert!(err.contains("the embedding model BAAI/bge-small-en-v1.5 did not load"), "{err}");
    assert!(canary.hits() > 0, "the canary sees the download attempt");
}

// DM_REQUIRE_MODEL=0 (also false, no) opens a local tenant on the placeholder with one loud line
// that memory recall is degraded, and doctor reports "degraded: placeholder embedder".
#[cfg(feature = "candle")]
#[test]
fn opting_out_opens_a_local_tenant_on_the_placeholder_with_a_warning() {
    let canary = Canary::start();
    let root = scratch("modeloptout");
    for (i, off) in ["0", "false", "no"].into_iter().enumerate() {
        let mut env = no_model_env(&root, &canary);
        env.extend([("HF_HUB_OFFLINE", "1".to_string()), ("DM_REQUIRE_MODEL", off.to_string())]);
        let env = as_refs(&env);
        let note = format!("zmo placeholder note {i}");
        let (st, out, err) = run_dmem(&root, &env, &["remember", &note], "");
        assert!(st.success(), "DM_REQUIRE_MODEL={off}: {out}{err}");
        assert_eq!(err.matches("WARNING memory recall is degraded").count(), 1, "{err}");
        assert_eq!(err.lines().filter(|l| l.contains("WARNING")).count(), 1, "one line: {err}");
        assert!(err.contains("placeholder hash embedder is active"), "{err}");
        assert!(err.contains("BAAI/bge-small-en-v1.5 did not load"), "{err}");
        let (st, out, err) = run_dmem(&root, &env, &["recall", "zmo placeholder"], "");
        assert!(st.success() && out.contains(&note), "{out}{err}");
        let (st, out, _) = run_dmem(&root, &env, &["doctor"], "");
        assert!(st.success() && out.contains("degraded: placeholder embedder"), "{out}");
    }
    assert_eq!(canary.hits(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

// Remote-client mode is unaffected: with a [server] block the client never loads or downloads a
// model and does not fail for lack of one, though the model is required (the default) and none
// is anywhere on the client. The server embeds (here on the placeholder, opted out).
#[cfg(feature = "client")]
#[test]
fn a_remote_client_never_loads_the_model() {
    let canary = Canary::start();
    let srv_root = scratch("rc-server");
    let srv_env = model_unavailable_env(&srv_root);
    let mut opted_out = as_refs(&srv_env);
    opted_out.push(("DM_REQUIRE_MODEL", "0"));
    let mut s = Serve::start(&srv_root, &opted_out, &[]);
    s.wait_healthy(300);

    let root = scratch("rc-client");
    let config = root.join("config.toml");
    std::fs::write(&config, format!("[server]\nurl = \"http://{}\"\ntoken = \"{TENANT_TOKEN}\"\n", s.addr)).unwrap();
    let mut env = no_model_env(&root, &canary); // online: a model load would reach the canary
    env.push(("DM_CONFIG", config.display().to_string()));
    let env = as_refs(&env);
    let mut stderr = String::new();
    let (st, out, err) = run_dmem(&root, &env, &["remember", "zrc remote note"], "");
    assert!(st.success(), "{out}{err}");
    stderr.push_str(&err);
    let (st, out, err) = run_dmem(&root, &env, &["recall", "zrc remote"], "");
    assert!(st.success() && out.contains("zrc remote note"), "{out}{err}");
    stderr.push_str(&err);
    let (st, out, err) = run_dmem(&root, &env, &["mcp"], MCP_PING);
    assert!(st.success() && out.contains(r#""result""#), "{out}{err}");
    stderr.push_str(&err);
    let (st, out, err) = run_dmem(&root, &env, &["skills", "list"], "");
    assert!(st.success(), "{out}{err}");
    stderr.push_str(&err);
    let (st, out, err) = run_dmem(&root, &env, &["doctor", "--json"], "");
    assert!(st.success(), "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["mode"], "remote client", "{v}");
    assert!(v["model_state"].as_str().unwrap().starts_with("n/a"), "{v}");
    for word in ["embedder", "did not load", "placeholder", "WARNING"] {
        assert!(!stderr.contains(word), "{word:?} on the client:\n{stderr}");
    }
    assert_eq!(canary.hits(), 0, "the client made no download attempt");
    assert!(!root.join("home/.cache").exists(), "no model cache on the client");
    assert!(!root.join("data/vectors").exists(), "no local engine opened");
    s.kill();
    let _ = std::fs::remove_dir_all(&root);
}

// The model cache follows HUGGINGFACE_HUB_CACHE, then HF_HOME (as HF_HOME/hub). With the model
// only there, an empty home and HF_HUB_OFFLINE=1, a local tenant opens and `dmem serve` starts
// on the real model, and doctor reports that cache. 0.3.7 read only ~/.cache/huggingface/hub.
#[cfg(feature = "candle")]
#[test]
fn hf_home_and_huggingface_hub_cache_locate_the_model() {
    let canary = Canary::start();
    let root = scratch("hfhome");
    let hf_home = root.join("hf");
    mirror_model_cache(&hf_home.join("hub"));
    let mut env = no_model_env(&root, &canary);
    env.extend([("HF_HUB_OFFLINE", "1".to_string()), ("HF_HOME", hf_home.display().to_string())]);
    let env = as_refs(&env);
    let (st, out, err) = run_dmem(&root, &env, &["doctor", "--json"], "");
    assert!(st.success(), "{out}{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["model_cache"], hf_home.join("hub").display().to_string(), "{v}");
    assert_eq!(v["model_cached"], true, "{v}");
    assert!(v["model_state"].as_str().unwrap().starts_with("ok"), "{v}");
    let (st, out, err) = run_dmem(&root, &env, &["remember", "zhf note on the real model"], "");
    assert!(st.success(), "{out}{err}");
    assert!(!err.contains("WARNING") && !err.contains("did not load"), "{err}");

    // an HF_HOME without the model: refused, naming HF_HOME/hub
    let empty = root.join("empty-hf");
    std::fs::create_dir_all(&empty).unwrap();
    let mut env = no_model_env(&root, &canary);
    env.extend([("HF_HUB_OFFLINE", "1".to_string()), ("HF_HOME", empty.display().to_string())]);
    let (st, out, err) = run_dmem(&root, &as_refs(&env), &["remember", "zhf refused note"], "");
    assert!(!st.success(), "{out}{err}");
    assert!(err.contains(&format!("Model cache: {} (the model is not there", empty.join("hub").display())), "{err}");

    // HUGGINGFACE_HUB_CACHE wins over HF_HOME (the empty one), and serve starts on the model
    let root2 = scratch("hubcache");
    let hub = root2.join("hubcache");
    mirror_model_cache(&hub);
    let mut env = no_model_env(&root2, &canary);
    env.extend([
        ("HF_HUB_OFFLINE", "1".to_string()),
        ("HF_HOME", empty.display().to_string()),
        ("HUGGINGFACE_HUB_CACHE", hub.display().to_string()),
    ]);
    let mut s = Serve::start(&root2, &as_refs(&env), &[]);
    s.wait_healthy(300);
    assert!(s.log().contains(&format!("model=BAAI/bge-small-en-v1.5 cache={} (cached)", hub.display())), "{}", s.log());
    assert!(!s.log().contains("WARNING memory recall is degraded"), "{}", s.log());
    let (st, body) = readyz_settled(&s.addr);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains(r#""embedder":"pass""#), "{body}");
    s.kill();
    assert_eq!(canary.hits(), 0, "offline: loaded from the cache alone");
    let _ = std::fs::remove_dir_all(&root);
}
