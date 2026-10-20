//! Process-level tests of the fence on `dmem upgrade` (docs/managed-install.md): the real
//! binary, copied into a directory of its own, run with and without the marker. What the unit
//! tests of `fence` cannot see: that a refusal is the process's exit status 3 and its stderr,
//! that `--yes` does not get past it, that it holds through a symlink, and that a refused
//! upgrade opens no connection at all.
//!
//! "No connection" is measured, not assumed. The child is told to use a proxy on a local port
//! for every request, and a listener there counts the connections. An install that is not
//! fenced must reach that listener (the control test); a fenced one must not.
//!
//! The `Upgrade` command only exists in a build with the `self-update` feature (the `dist`
//! build has it), so these tests run only there.
#![cfg(all(unix, feature = "self-update"))]

use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The marker an installer writes beside the real binary, and the variable (docs/managed-install.md).
const MARKER: &str = ".daimon-managed";
const VAR: &str = "DM_MANAGED";

static N: AtomicU64 = AtomicU64::new(0);

/// A fresh directory for one test, removed when the test ends (also when it fails).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("dmfence-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An installed `dmem`: a copy of the binary under test in `real/`, and a symlink to it from
/// `link/`, as `/usr/local/bin/dmem` would point into an installer's directory.
struct Install {
    scratch: Scratch,
    real: PathBuf,
    link: PathBuf,
}

impl Install {
    fn new(tag: &str) -> Install {
        let scratch = Scratch::new(tag);
        let real_dir = scratch.0.join("real");
        let link_dir = scratch.0.join("link");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let real = real_dir.join("dmem");
        std::fs::copy(env!("CARGO_BIN_EXE_dmem"), &real).unwrap();
        let link = link_dir.join("dmem");
        symlink(&real, &link).unwrap();
        Install { scratch, real, link }
    }

    /// The marker, beside the real file.
    fn mark_managed(&self) {
        std::fs::write(self.real.parent().unwrap().join(MARKER), b"").unwrap();
    }
}

/// A listener on a free local port that counts the connections made to it and drops each.
struct Proxy {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    fn start() -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        let thread = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok(_) => {
                    h.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if s.load(Ordering::SeqCst) {
                        // one last look: a connection made before the stop is still queued
                        while listener.accept().is_ok() {
                            h.fetch_add(1, Ordering::SeqCst);
                        }
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        });
        Proxy { port, hits, stop, thread: Some(thread) }
    }

    /// The number of connections made so far; call it after the process under test has exited.
    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        self.hits.load(Ordering::SeqCst)
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    connections: usize,
}

/// Run `exe args` with every request routed to a counting listener, the child's own config and
/// data pointing nowhere, and `env` applied last (`None` removes a variable). A child that does
/// not end within a minute is killed.
fn run(install: &Install, exe: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Run {
    let proxy = Proxy::start();
    let url = format!("http://127.0.0.1:{}", proxy.port);
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env("HOME", install.scratch.0.join("home"))
        .env("DM_CONFIG", install.scratch.0.join("no-config.toml"))
        .env("DM_DATA_DIR", install.scratch.0.join("data"))
        .env_remove(VAR)
        .env_remove("DM_ENDPOINT")
        .env_remove("DM_TOKEN")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        cmd.env(k, &url);
    }
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = spawn(&mut cmd);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`dmem {}` did not end within a minute", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (mut stdout, mut stderr) = (String::new(), String::new());
    if let Some(mut p) = child.stdout.take() {
        let _ = p.read_to_string(&mut stdout);
    }
    if let Some(mut p) = child.stderr.take() {
        let _ = p.read_to_string(&mut stderr);
    }
    Run { code: status.code(), stdout, stderr, connections: proxy.finish() }
}

/// `spawn`, again for a moment when the kernel says the file is busy: another test's `fork`
/// can still hold the write end of the binary this one just copied (ETXTBSY).
fn spawn(cmd: &mut Command) -> std::process::Child {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Ok(c) => return c,
            Err(e) if e.raw_os_error() == Some(26) && tries < 100 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("cannot start dmem: {e}"),
        }
    }
}

/// A refused upgrade: exit status 3, the installer named, no output of an upgrade, no connection.
fn assert_refused(r: &Run, what: &str) {
    assert_eq!(r.code, Some(3), "{what}: exit status; stdout {:?}, stderr {:?}", r.stdout, r.stderr);
    assert!(r.stderr.contains("dmem upgrade refused"), "{what}: {:?}", r.stderr);
    assert!(r.stderr.contains("daimon-expert upgrade"), "{what}: {:?}", r.stderr);
    assert!(r.stdout.is_empty(), "{what}: printed {:?}", r.stdout);
    assert_eq!(r.connections, 0, "{what}: a refused upgrade connected");
}

#[test]
fn the_marker_beside_the_real_file_stops_every_form_of_upgrade() {
    let i = Install::new("marker");
    i.mark_managed();
    for exe in [&i.real, &i.link] {
        for args in [&["upgrade"][..], &["upgrade", "--yes"], &["upgrade", "-y"], &["upgrade", "--pre", "--yes"]] {
            let r = run(&i, exe, args, &[]);
            assert_refused(&r, &format!("{} {}", exe.display(), args.join(" ")));
        }
    }
}

#[test]
fn the_variable_stops_upgrade_where_there_is_no_marker() {
    let i = Install::new("variable");
    for v in ["1", "true", "On", " yes "] {
        let r = run(&i, &i.real, &["upgrade", "--yes"], &[(VAR, Some(v))]);
        assert_refused(&r, &format!("{VAR}={v:?}"));
        assert!(r.stderr.contains(VAR), "the message names the variable: {:?}", r.stderr);
    }
}

#[test]
fn an_install_nothing_marks_goes_on_to_its_usual_checks() {
    // The control: without it, "no connection" above could just mean "no network in this test".
    let i = Install::new("control");
    for env in [None, Some("0"), Some("false"), Some("")] {
        let r = run(&i, &i.link, &["upgrade", "--yes"], &[(VAR, env)]);
        assert_ne!(r.code, Some(3), "{env:?}: not refused; stderr {:?}", r.stderr);
        assert!(r.connections >= 1, "{env:?}: it never tried the network; stderr {:?}", r.stderr);
    }
}

#[test]
fn the_marker_fences_upgrade_only() {
    let i = Install::new("scope");
    i.mark_managed();
    let r = run(&i, &i.real, &["--version"], &[]);
    assert_eq!(r.code, Some(0), "stderr {:?}", r.stderr);
    assert!(r.stdout.starts_with("dmem "), "{:?}", r.stdout);
}
