//! Process-level tests of the vector index open probe (zvec_index::probe): a child process opens
//! an index that the last run did not close cleanly, so damage zvec's native open would crash on
//! costs the child, not the server. What the unit tests cannot cover: the probe's children are
//! real processes (forking them inside the unit-test binary would leak the descriptors of the
//! indexes other tests have open), and the end-to-end path runs the real `dmem` binary.
//!
//! The probe only exists in a build with the vector index (`zvec`).
#![cfg(all(unix, feature = "zvec"))]

use dm_lite::zvec_index::{dirty_marker, probe, probe_many, probe_with, Probe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static N: AtomicU64 = AtomicU64::new(0);

/// These tests fork children on purpose. Forking while another test writes its stand-in script
/// keeps that file open in the child for an instant ("Text file busy" on its exec), so they run
/// one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn scratch(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("dmprobe-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A stand-in for the probe's child: a shell script doing `body`.
fn fake_child(base: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = base.join(name);
    std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

#[test]
fn the_probe_reports_a_crash_a_failure_a_hang_and_a_clean_open() {
    let _serial = serial();
    let base = scratch("parent");
    let idx = base.join("t");
    std::fs::create_dir_all(&idx).unwrap();
    let t = Duration::from_secs(10);
    assert_eq!(probe_with(&idx, &fake_child(&base, "ok", "exit 0"), t), Probe::Opened);
    // What zvec's native open did on 27-10-2026: the child dies with SIGSEGV.
    assert_eq!(
        probe_with(&idx, &fake_child(&base, "segv", "kill -SEGV $$"), t),
        Probe::Failed("opening it killed the probe with signal 11".into())
    );
    assert_eq!(
        probe_with(&idx, &fake_child(&base, "fail", "exit 1"), t),
        Probe::Failed("the probe could not open it (exit 1)".into())
    );
    let start = Instant::now();
    // `exec`, so the process killed at the timeout is the one that is running.
    let hang = probe_with(&idx, &fake_child(&base, "hang", "exec sleep 30"), Duration::from_millis(300));
    assert_eq!(hang, Probe::Failed("opening it did not finish within 0 s".into()));
    assert!(start.elapsed() < Duration::from_secs(10), "a hung child is killed, not waited for");
    assert!(matches!(probe_with(&idx, &base.join("no-such-program"), t), Probe::Failed(_)));
    // The child's "another process holds the lock": in use, not damaged.
    assert_eq!(probe_with(&idx, &fake_child(&base, "inuse", "exit 75"), t), Probe::InUse);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_probe_child_inherits_no_descriptor_of_the_parent() {
    let _serial = serial();
    // zvec opens its files without close-on-exec; a probe child holding them would keep another
    // index's LOCK file open after the parent closed it. Imitate zvec: a descriptor with its
    // close-on-exec flag cleared, then let the child list its own descriptors.
    use std::os::fd::AsRawFd;
    let base = scratch("fds");
    let held = std::fs::File::open(&base).unwrap();
    // dup2 to a number well clear of the child's own descriptors; dup2 leaves close-on-exec
    // cleared on the copy, as zvec leaves it on its files.
    let fd: i32 = 50;
    // SAFETY: dup2/fcntl on descriptors this test owns (50 is closed again below).
    unsafe {
        assert_eq!(libc::dup2(held.as_raw_fd(), fd), fd);
        assert_eq!(libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    }
    let out = base.join("fds.txt");
    let child = fake_child(&base, "list", &format!("exec ls /proc/self/fd > {}", out.display()));
    assert_eq!(probe_with(&base, &child, Duration::from_secs(10)), Probe::Opened);
    let fds: Vec<i32> = std::fs::read_to_string(&out).unwrap().split_whitespace().filter_map(|f| f.parse().ok()).collect();
    assert!(!fds.contains(&fd), "the probe child inherited descriptor {fd}: {fds:?}");
    // SAFETY: closes the copy made above.
    unsafe { libc::close(fd) };
    drop(held);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn many_dirty_indexes_are_probed_concurrently_within_a_bounded_time() {
    // After a power-off every index is dirty. Probed one by one, 8 hanging probes would take
    // 8 x the timeout; four at a time they take two waves.
    let _serial = serial();
    let base = scratch("many");
    let hang = fake_child(&base, "hang", "exec sleep 30");
    let dirs: Vec<PathBuf> = (0..8)
        .map(|i| {
            let d = base.join(format!("t{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(dirty_marker(&d), "dirty\n").unwrap();
            d
        })
        .collect();
    std::env::set_var("DM_VECTOR_PROBE_EXE", &hang);
    std::env::set_var("DM_VECTOR_PROBE_TIMEOUT_SECS", "1");
    std::env::set_var("DM_VECTOR_PROBE_CONCURRENCY", "4");
    let start = Instant::now();
    assert_eq!(probe_many(&dirs), 8);
    let took = start.elapsed();
    assert!(took < Duration::from_secs(5), "8 hanging probes, 4 at a time, 1 s each: two waves, not eight ({took:?})");
    assert!(took >= Duration::from_secs(1), "the probes did run ({took:?})");
    // Each verdict is kept for its index's open, so opening does not probe again.
    for d in &dirs {
        let start = Instant::now();
        assert_eq!(probe(d), Probe::Failed("opening it did not finish within 1 s".into()));
        assert!(start.elapsed() < Duration::from_millis(500), "the kept verdict, not a new probe");
    }
    // A clean start: no dirty marker, nothing probed, no wait.
    for d in &dirs {
        std::fs::remove_file(dirty_marker(d)).unwrap();
    }
    let start = Instant::now();
    assert_eq!(probe_many(&dirs), 0);
    assert!(start.elapsed() < Duration::from_millis(200));
    for k in ["DM_VECTOR_PROBE_EXE", "DM_VECTOR_PROBE_TIMEOUT_SECS", "DM_VECTOR_PROBE_CONCURRENCY"] {
        std::env::remove_var(k);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// `dmem` run with a private store: local mode, no model (the placeholder embedder), offline.
fn dmem(data: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dmem"));
    c.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", data)
        .env("DM_CONFIG", "/dev/null")
        .env("DM_DATA_DIR", data)
        .env("DM_TENANT", "t")
        .env("DM_REQUIRE_MODEL", "0")
        .env("HF_HOME", data.join("hf"))
        .env("HF_HUB_OFFLINE", "1");
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn newest_manifest(vdir: &Path) -> PathBuf {
    std::fs::read_dir(vdir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.file_name().to_string_lossy().strip_prefix("manifest.")?.parse::<u64>().ok()?, e.path())))
        .max_by_key(|(n, _)| *n)
        .unwrap()
        .1
}

#[test]
fn a_clean_run_leaves_no_dirty_marker() {
    let _serial = serial();
    let data = scratch("clean");
    assert!(dmem(&data, &["remember", "zebra pinned certificate rotation note"], &[]).status.success());
    let vdir = data.join("vectors").join("t");
    assert!(vdir.exists());
    assert!(!dirty_marker(&vdir).exists(), "a process that closes its index cleanly removes the marker");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_cli_run_beside_a_running_server_never_quarantines_the_servers_index() {
    // A running server keeps its index's dirty marker (the index is open). A CLI command on the
    // same data dir then sees the marker and probes; the probe finds the index locked. That is
    // "in use", and quarantining it would pull the index out from under the live server.
    let _serial = serial();
    let data = scratch("inuse");
    assert!(dmem(&data, &["remember", "zebra pinned certificate rotation note"], &[]).status.success());
    let port = 20000 + (std::process::id() % 20000) as u16;
    let mut server = Command::new(env!("CARGO_BIN_EXE_dmem"))
        .args(["serve", "--addr", &format!("127.0.0.1:{port}")])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &data)
        .env("DM_CONFIG", "/dev/null")
        .env("DM_DATA_DIR", &data)
        .env("DM_REQUIRE_MODEL", "0")
        .env("HF_HOME", data.join("hf"))
        .env("HF_HUB_OFFLINE", "1")
        .env("DM_TOKEN_T", "inusetok-0123456789")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let vdir = data.join("vectors").join("t");
    let start = Instant::now();
    while !dirty_marker(&vdir).exists() && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(dirty_marker(&vdir).exists(), "the server has the index open");
    let out = dmem(&data, &["recall", "zebra certificate"], &[]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = server.kill();
    let _ = server.wait();
    let quarantined = std::fs::read_dir(data.join("vectors"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().starts_with("t.corrupt-"));
    assert!(!quarantined, "the server's index was quarantined by a CLI run: {err}");
    assert!(!err.contains("failed its open probe"), "{err}");
    assert!(vdir.exists());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn an_index_left_dirty_and_unreadable_is_quarantined_and_rebuilt() {
    let _serial = serial();
    // A manifest that is damaged but not empty: the size check cannot see it, and zvec then
    // fails to open the index. Without the probe the tenant stayed keyword-only for good; with
    // it, the index is moved aside and rebuilt from the database.
    let data = scratch("e2e");
    for text in ["zebra pinned certificate rotation note", "quartz backup restore drill for the archive"] {
        assert!(dmem(&data, &["remember", text], &[]).status.success());
    }
    let vdir = data.join("vectors").join("t");
    let m = newest_manifest(&vdir);
    let len = std::fs::metadata(&m).unwrap().len() as usize;
    std::fs::write(&m, vec![0x5au8; len]).unwrap();
    std::fs::write(dirty_marker(&vdir), "left by a killed process\n").unwrap();
    let out = dmem(&data, &["recall", "zebra certificate rotation"], &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("failed its open probe after an unclean stop"), "{err}");
    let quarantined = std::fs::read_dir(data.join("vectors"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().starts_with("t.corrupt-"));
    assert!(quarantined, "the damaged index is kept beside the new one");
    assert!(String::from_utf8_lossy(&out.stdout).contains("zebra pinned certificate rotation note"));
    // The new index holds both records again: a semantic query finds the other one too.
    let out = dmem(&data, &["recall", "quartz archive restore"], &[]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("quartz backup restore drill"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!dirty_marker(&vdir).exists());
    let _ = std::fs::remove_dir_all(&data);
}
