//! zvec-backed vector index (Alibaba zvec, in-process). The project's chosen vector substrate
//! (picked over the earlier LanceDB recommendation). Stores one embedding slot per RECORD -
//! a (uri, scope) pair, since the same uri can be held by several scopes - and search returns
//! the nearest slots. Behind the `zvec` feature; SQLite stays the canonical record + keyword
//! store, this is the dense-vector half fused via RRF in recall.

use crate::embedder::DIM;
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::Once;
use zvec::*;

static INIT: Once = Once::new();

fn ze<E: std::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow!("zvec: {:?}", e)
}

/// zvec's primary key is capped at 64 bytes and rejects `:` / `/` (it reports both as
/// "contains invalid characters"), so a daimon:// URI can't be the PK directly. We derive a
/// short, fixed-length, charset-safe PK by hashing the URI; the real URI is stored in the
/// "uri" string field (string field values are unrestricted) and read back on search. The
/// hash is deterministic, so re-saving the same URI supersedes its prior vector.
fn pk_for(uri: &str) -> String {
    // 128-bit FNV-1a (two independent streams) -> 32 hex chars. Well under the 64-byte cap;
    // collision-free at our scale (tens-to-hundreds of records per tenant).
    fn fnv1a(seed: u64, s: &str) -> u64 {
        let mut h: u64 = seed;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
    let a = fnv1a(0xcbf29ce484222325, uri);
    let b = fnv1a(0x84222325cbf29ce4, uri);
    format!("{a:016x}{b:016x}")
}

/// A scoped record's slot id: SHA-256 of `s<len>:<scope><uri>`, truncated to 128 bits (32 hex
/// chars, the same length as `pk_for`). The key carries the scope, length-prefixed so the
/// encoding is injective (a uri never starts that way). A slot is a scope boundary - one
/// scope's save or forget must never reach another scope's vector - and a uri's namespace is
/// free text, so these ids use a cryptographic hash: a crafted title cannot be made to collide
/// with another record's slot. (FNV, which `pk_for` keeps for the global slot, is not built to
/// resist that.) Scoped slots first appear in 0.3.7, so no stored id depends on the old form.
fn scoped_pk(uri: &str, scope: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("s{}:{scope}{uri}", scope.len()).as_bytes());
    digest[..SLOT_LEN / 2].iter().map(|b| format!("{b:02x}")).collect()
}

/// The slot id (the chunk-0 PK) of a (uri, scope) record: what `search` reports in
/// `VectorHit::slot`, so a hit can be matched back to the one record it embeds. A GLOBAL record
/// keeps the bare-uri key - the historical one-vector-per-uri PK, so every global vector written
/// before per-scope slots existed is already in its right place. A scoped record has its own
/// slot (`scoped_pk`). Without per-scope slots a room re-saving a global record's title
/// overwrote the global vector, and hybrid recall ranked the global record by the room's text.
pub fn slot_of(uri: &str, scope: &str) -> String {
    if scope.is_empty() {
        pk_for(uri)
    } else {
        scoped_pk(uri, scope)
    }
}

/// A record's body may span several embedding windows (see Embedder::embed_chunks). Each window is
/// a distinct vector under its own PK, but all carry the same `uri` field, so search max-pools them
/// back to one record. Chunk 0's PK IS the slot id (for a global record, `pk_for(uri)`: the
/// historical single-vector key), so a single-chunk record is byte-identical to the pre-chunking
/// scheme; chunks 1.. append a 2-hex suffix (PK stays well under zvec's 64-byte cap).
const MAX_CHUNKS: usize = 64;
fn chunk_pk(uri: &str, scope: &str, i: usize) -> String {
    let slot = slot_of(uri, scope);
    if i == 0 {
        slot
    } else {
        format!("{slot}{i:02x}")
    }
}

/// Length of a slot id (and so of a chunk-0 PK); later chunks append two hex chars.
const SLOT_LEN: usize = 32;

/// Why the index under `dir` cannot be opened safely, if it cannot: it has no `manifest.N`
/// file, or its newest one is empty. That is what an unclean stop leaves when it cuts a flush
/// off mid-write (a power-off on 27-10-2026 left `manifest.3` at 0 bytes), and zvec's native
/// open does not fail on it: the process dies with SIGSEGV, on every restart. None for a
/// directory that does not exist (a fresh index) or looks whole. A cheap check on file names
/// and sizes only; it cannot see a manifest that is torn but not empty.
pub fn torn_reason(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let newest = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u64 = name.strip_prefix("manifest.")?.parse().ok()?;
            Some((n, e.metadata().map(|m| m.len()).unwrap_or(0)))
        })
        .max_by_key(|(n, _)| *n);
    match newest {
        None => Some("it has no manifest".to_string()),
        Some((n, 0)) => Some(format!("its newest manifest, manifest.{n}, is empty")),
        Some(_) => None,
    }
}

/// The marker of an index that is open, or was when its process last stopped: written when
/// `ZvecIndex::open` opens it, removed when the handle closes cleanly (flushed and synced). A
/// marker found at open means the last run did not close it, so the index may be torn in a way
/// `torn_reason` cannot see, and `probe` opens it in a child process first.
pub fn dirty_marker(dir: &Path) -> std::path::PathBuf {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.with_file_name(format!("{name}.dirty"))
}

/// What `probe` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The index opened in the child: safe to open here.
    Opened,
    /// The child could not open it: why (a signal, a failed open, or the timeout).
    Failed(String),
    /// No probe was run (no `dmem` binary to run it with, or `DM_VECTOR_PROBE=0`).
    Skipped(&'static str),
    /// Another process has the index open (its lock is held): it is in use, not damaged. A
    /// running server keeps its index's dirty marker, so a CLI run beside it lands here.
    InUse,
}

/// The probe child's exit status when the index is locked by another process (EX_TEMPFAIL).
pub const PROBE_IN_USE: i32 = 75;

/// The hidden `dmem` subcommand a probe runs.
pub const PROBE_SUBCOMMAND: &str = "__probe_vector_index";

/// Open the index under `dir` in a child process (`dmem __probe_vector_index <dir>`), so a
/// native crash in zvec's open (a torn manifest that is not empty, or any other damage) kills
/// the child instead of this process. Bounded: `DM_VECTOR_PROBE_TIMEOUT_SECS` (default 60), and
/// a child still running then is killed and counted as a failure. The program is this process's
/// own binary when it is `dmem` (an embedding product's binary does not have the subcommand),
/// or `DM_VECTOR_PROBE_EXE`; `DM_VECTOR_PROBE=0` turns probing off. A verdict `probe_many`
/// already reached for `dir` in this process is used instead of probing again.
pub fn probe(dir: &Path) -> Probe {
    if let Some(v) = take_verdict(dir) {
        return v;
    }
    if std::env::var("DM_VECTOR_PROBE").is_ok_and(|v| ["0", "off", "false", "no"].contains(&v.trim().to_ascii_lowercase().as_str())) {
        return Probe::Skipped("DM_VECTOR_PROBE is off");
    }
    let exe = match std::env::var_os("DM_VECTOR_PROBE_EXE").map(std::path::PathBuf::from) {
        Some(p) => p,
        None => match std::env::current_exe() {
            Ok(p) if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("dmem")) => p,
            _ => return Probe::Skipped("no dmem binary to probe with"),
        },
    };
    probe_with(dir, &exe, probe_timeout())
}

/// How long one probe may take: `DM_VECTOR_PROBE_TIMEOUT_SECS`, default 20. A whole index
/// normally opens in well under a second, so a child still running at the limit is stuck.
fn probe_timeout() -> std::time::Duration {
    let secs = std::env::var("DM_VECTOR_PROBE_TIMEOUT_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(20);
    std::time::Duration::from_secs(secs.max(1))
}

/// How many probes `probe_many` runs at once: `DM_VECTOR_PROBE_CONCURRENCY`, default 4.
fn probe_concurrency() -> usize {
    std::env::var("DM_VECTOR_PROBE_CONCURRENCY").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(4).max(1)
}

/// Verdicts `probe_many` reached, waiting for the open of their index (`probe` takes them).
static VERDICTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Probe>>> = std::sync::OnceLock::new();

fn take_verdict(dir: &Path) -> Option<Probe> {
    VERDICTS.get()?.lock().unwrap_or_else(|p| p.into_inner()).remove(dir)
}

/// Probe every index in `dirs` that has a dirty marker, up to `DM_VECTOR_PROBE_CONCURRENCY` at a
/// time, and keep each verdict for that index's open. `dmem serve` runs it before opening its
/// tenants: after a power-off every index is dirty, and probing them one after another (each
/// bounded by `DM_VECTOR_PROBE_TIMEOUT_SECS`) could hold readiness for minutes. Bounded: about
/// ceil(dirty / concurrency) x timeout in the worst case. A clean start probes nothing.
/// Returns how many indexes were probed.
pub fn probe_many(dirs: &[std::path::PathBuf]) -> usize {
    let dirty: Vec<&std::path::PathBuf> = dirs.iter().filter(|d| d.exists() && dirty_marker(d).exists()).collect();
    if dirty.is_empty() {
        return 0;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let verdicts = VERDICTS.get_or_init(Default::default);
    std::thread::scope(|scope| {
        for _ in 0..probe_concurrency().min(dirty.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(dir) = dirty.get(i) else { break };
                let v = probe(dir);
                verdicts.lock().unwrap_or_else(|p| p.into_inner()).insert((*dir).clone(), v);
            });
        }
    });
    dirty.len()
}

/// `probe` with the program and the timeout given (the env lookups done by the caller).
pub fn probe_with(dir: &Path, exe: &Path, timeout: std::time::Duration) -> Probe {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(PROBE_SUBCOMMAND)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // zvec opens its files without close-on-exec, so a child would inherit the descriptors of
    // every index this process has open, LOCK files included: an index closed here while the
    // child lives would stay locked. Mark every inherited descriptor close-on-exec (not closed:
    // std's own exec-error pipe must stay usable until the exec).
    // SAFETY: runs in the forked child before exec and makes only async-signal-safe calls
    // (close_range, fcntl).
    unsafe {
        cmd.pre_exec(|| {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
            if libc::syscall(libc::SYS_close_range, 3 as libc::c_uint, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) != 0 {
                for fd in 3..4096 {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    }
                }
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Probe::Failed(format!("the probe could not start ({e})")),
    };
    let end = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Probe::Opened,
            Ok(Some(status)) if status.code() == Some(PROBE_IN_USE) => return Probe::InUse,
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                return Probe::Failed(match status.signal() {
                    Some(sig) => format!("opening it killed the probe with signal {sig}"),
                    None => format!("the probe could not open it (exit {})", status.code().unwrap_or(-1)),
                });
            }
            Ok(None) if std::time::Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe::Failed(format!("opening it did not finish within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => return Probe::Failed(format!("the probe could not be watched ({e})")),
        }
    }
}

/// The probe's child side (`dmem __probe_vector_index <dir>`): open the index and exit 0. A
/// crash here is what the parent is looking for. No marker is written or removed.
/// The probe's child side (`dmem __probe_vector_index <dir>`): open the index and exit 0. A
/// crash here is what the parent is looking for. An index another process holds open exits with
/// `PROBE_IN_USE`, so the parent never mistakes "in use" for "damaged". No marker is written or
/// removed.
pub fn probe_child(dir: &Path) -> Result<()> {
    INIT.call_once(|| {
        let _ = initialize(None);
    });
    match Collection::open(&dir.to_string_lossy(), None).map_err(ze) {
        Ok(collection) => {
            drop(collection);
            Ok(())
        }
        Err(e) if is_lock_error(&e) => std::process::exit(PROBE_IN_USE),
        Err(e) => Err(e),
    }
}

/// zvec's refusal to open a collection another process has locked.
fn is_lock_error(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("Can't lock")
}

/// Move a torn index aside, next to where it was: `<name>.corrupt-<UTC time>` (with a counter
/// when that name is taken), so nothing is lost and a fresh index can take its place. A rename
/// on the same filesystem, so it is atomic. Returns where it went.
pub fn quarantine(dir: &Path) -> Result<std::path::PathBuf> {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| anyhow!("no index directory name"))?;
    let stamp = utc_stamp(std::time::SystemTime::now());
    let mut to = dir.with_file_name(format!("{name}.corrupt-{stamp}"));
    let mut i = 2;
    while to.exists() {
        to = dir.with_file_name(format!("{name}.corrupt-{stamp}-{i}"));
        i += 1;
    }
    std::fs::rename(dir, &to).map_err(|e| anyhow!("move {} to {}: {e}", dir.display(), to.display()))?;
    Ok(to)
}

/// `YYYYMMDD-HHMMSSZ` in UTC, for file names (civil date from days, H. Hinnant).
fn utc_stamp(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// One vector-search hit: the record's uri, its slot id (`slot_of(uri, scope)` of the record the
/// vector embeds - match it against the uri's live scopes to learn which record), and the cosine
/// similarity of its best-matching chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorHit {
    pub uri: String,
    pub slot: String,
    pub score: f32,
}

pub struct ZvecIndex {
    collection: Collection,
    /// Where the index lives, for syncing it after a flush.
    dir: Option<std::path::PathBuf>,
}

impl Drop for ZvecIndex {
    /// A clean close: flush and sync, then drop the dirty marker. Skipped while panicking, and
    /// the marker stays when the flush fails, so the next open probes the index.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if self.flush().is_ok() {
            if let Some(dir) = &self.dir {
                let _ = std::fs::remove_file(dirty_marker(dir));
            }
        }
    }
}

/// fsync every file under `dir`, then each directory (so new names are durable too). LOCK
/// files are skipped: they hold no data, and they carry the open index's locks, which a POSIX
/// record lock loses when ANY descriptor of the file closes, so opening one here could release
/// the index's own lock while it is still open.
fn sync_tree(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            sync_tree(&path)?;
        } else if path.file_name().is_some_and(|n| n == "LOCK") {
            continue;
        } else if let Ok(f) = std::fs::File::open(&path) {
            f.sync_all()?;
        }
    }
    std::fs::File::open(dir)?.sync_all()
}

impl ZvecIndex {
    /// Open the vector collection under `dir`, creating it the first time. zvec's
    /// `create_and_open` rejects an existing path, so we create-or-open explicitly.
    pub fn open(dir: &Path) -> Result<Self> {
        INIT.call_once(|| {
            let _ = initialize(None);
        });
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let path = dir.to_string_lossy().to_string();
        let collection = if dir.exists() {
            Collection::open(&path, None).map_err(ze)?
        } else {
            let schema = CollectionSchema::builder("entries")
                .add_field(FieldSchema::new("uri", DataType::String, false, 0).map_err(ze)?)
                .add_vector_field(
                    "embedding",
                    DataType::VectorFp32,
                    DIM as u32,
                    IndexParams::hnsw(MetricType::Cosine, 16, 200).map_err(ze)?,
                )
                .build()
                .map_err(ze)?;
            Collection::create_and_open(&path, &schema, None).map_err(ze)?
        };
        // Open now: until a clean close removes it, the marker says the index may be dirty.
        let marker = dirty_marker(dir);
        if let Err(e) = std::fs::write(&marker, "this vector index is open, or its process stopped without closing it\n") {
            eprintln!("dmem: could not write {} ({e}); an unclean stop is not detected for this index", marker.display());
        }
        Ok(Self { collection, dir: Some(dir.to_path_buf()) })
    }

    /// Upsert (supersede) one record's vectors - the (uri, scope) slot - one per body window.
    /// Clears the slot's prior chunks first (including stale ones from a longer previous
    /// version), so re-saving is exact; other scopes' slots of the same uri are untouched. A
    /// single global vector reduces to the historical one-vector-per-uri behavior.
    pub fn upsert_chunks(&self, uri: &str, scope: &str, vectors: &[Vec<f32>]) -> Result<()> {
        // Best-effort clear the full chunk range so a shrink leaves no orphans.
        let stale: Vec<String> = (0..MAX_CHUNKS).map(|i| chunk_pk(uri, scope, i)).collect();
        let _ = self.collection.delete(&stale.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut docs = Vec::with_capacity(vectors.len());
        for (i, v) in vectors.iter().take(MAX_CHUNKS).enumerate() {
            let mut doc = Doc::new().map_err(ze)?;
            doc.set_pk(&chunk_pk(uri, scope, i));
            doc.add_string("uri", uri).map_err(ze)?;
            doc.add_vector_f32("embedding", v).map_err(ze)?;
            docs.push(doc);
        }
        let refs: Vec<&Doc> = docs.iter().collect();
        self.collection.insert(&refs).map_err(ze)?;
        Ok(())
    }

    /// Write the collection's buffered changes to disk. `dmem serve` calls it for every open
    /// tenant when it shuts down (SIGTERM), after in-flight requests have drained, so a
    /// planned stop leaves the index as current as the database beside it.
    ///
    /// zvec writes its files without syncing them, so after its flush every file of the index
    /// (and each directory) is synced too: a power-off right after a planned stop then cannot
    /// leave a manifest whose data never reached the disk. This does not cover zvec's own
    /// writes between flushes, which a power-off can still tear; the open guards against that
    /// (`torn_reason`, quarantine, rebuild from the database).
    pub fn flush(&self) -> Result<()> {
        self.collection.flush().map_err(ze)?;
        if let Some(dir) = &self.dir {
            if let Err(e) = sync_tree(dir) {
                eprintln!("dmem: vector index {} flushed but not synced ({e})", dir.display());
            }
        }
        Ok(())
    }

    /// Remove all chunk vectors of one record, the (uri, scope) slot (used by forget and by an
    /// invalidation that leaves the slot without a live row). Best-effort idempotent.
    pub fn remove(&self, uri: &str, scope: &str) -> Result<()> {
        let pks: Vec<String> = (0..MAX_CHUNKS).map(|i| chunk_pk(uri, scope, i)).collect();
        let _ = self.collection.delete(&pks.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        Ok(())
    }

    /// Nearest (uri, similarity) to the query vector, best first. zvec's `get_score()` for a
    /// MetricType::Cosine HNSW index returns a cosine DISTANCE (0.0 = identical, 1.0 = orthogonal,
    /// 2.0 = opposite; smaller = nearer), verified empirically. Our embeddings are L2-normalized,
    /// so we convert to a true cosine SIMILARITY = 1.0 - distance, giving [-1, 1] with HIGHER =
    /// more similar - the form the recall relevance floor (abs_cosine) thresholds on. Surfacing
    /// this magnitude (not just the uri) is what lets the floor drop semantically-distant hits.
    ///
    /// Hits are per RECORD - per (uri, scope) slot, not per uri: a uri held by two scopes can
    /// return two hits, and the caller decides (from the slot) which record each one embeds and
    /// whether the reader may see it.
    pub fn search(&self, vector: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        // A record can occupy several chunk vectors; over-fetch so that after collapsing chunks to
        // their parent record we still surface ~k distinct records.
        let raw_k = (k * 8).max(64);
        let q = SearchQuery::builder()
            .field_name("embedding")
            .vector(vector)
            .topk(raw_k as i32)
            .output_fields(&["uri"])
            .build()
            .map_err(ze)?;
        let results = self.collection.query(&q).map_err(ze)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in &results {
            if let Ok(Some(uri)) = r.get_string("uri") {
                // The slot is the chunk-0 PK: the first SLOT_LEN chars of any chunk's PK.
                let slot = match r.get_pk() {
                    Some(pk) if pk.len() >= SLOT_LEN => pk[..SLOT_LEN].to_string(),
                    _ => continue,
                };
                // First occurrence of a slot is its best chunk (results are best-first) -> max-pool.
                if seen.insert(slot.clone()) {
                    out.push(VectorHit { uri, slot, score: 1.0 - r.get_score() });
                    if out.len() >= k {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dmtorn-{tag}-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_torn_index_is_one_whose_newest_manifest_is_empty_or_missing() {
        let base = tmp("reason");
        let idx = base.join("t");
        assert_eq!(torn_reason(&idx), None, "no directory: a fresh index, not a torn one");
        std::fs::create_dir_all(idx.join("0")).unwrap();
        assert_eq!(torn_reason(&idx).as_deref(), Some("it has no manifest"));
        std::fs::write(idx.join("manifest.0"), [1u8; 100]).unwrap();
        assert_eq!(torn_reason(&idx), None);
        // The 27-10-2026 shape: older manifests whole, the newest one written as 0 bytes.
        std::fs::write(idx.join("manifest.2"), [1u8; 375]).unwrap();
        std::fs::write(idx.join("manifest.3"), b"").unwrap();
        assert_eq!(torn_reason(&idx).as_deref(), Some("its newest manifest, manifest.3, is empty"));
        // Numeric order, not text order: manifest.10 is newer than manifest.3.
        std::fs::write(idx.join("manifest.10"), [1u8; 200]).unwrap();
        assert_eq!(torn_reason(&idx), None);
        std::fs::write(idx.join("manifest.x"), b"").unwrap();
        assert_eq!(torn_reason(&idx), None, "names that are not manifest.<number> are ignored");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quarantine_moves_the_index_aside_and_never_overwrites() {
        let base = tmp("quar");
        let idx = base.join("tenant-a");
        std::fs::create_dir_all(&idx).unwrap();
        std::fs::write(idx.join("manifest.3"), b"").unwrap();
        let first = quarantine(&idx).unwrap();
        assert!(!idx.exists() && first.join("manifest.3").exists());
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("tenant-a.corrupt-") && name.ends_with('Z'), "{name}");
        std::fs::create_dir_all(&idx).unwrap();
        let second = quarantine(&idx).unwrap();
        assert_ne!(first, second, "a second quarantine in the same second gets its own name");
        assert!(first.exists() && second.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_stamp_is_utc_calendar_time() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_793_072_338); // 2026-10-27T03:38:58Z
        assert_eq!(utc_stamp(t), "20261027-033858Z");
        assert_eq!(utc_stamp(std::time::UNIX_EPOCH), "19700101-000000Z");
    }
    use super::*;

    /// (uri, score) per hit - the per-uri view the single-scope tests reason in.
    fn pairs(hits: Vec<VectorHit>) -> Vec<(String, f32)> {
        hits.into_iter().map(|h| (h.uri, h.score)).collect()
    }

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0f32; DIM];
        v[i] = 1.0;
        v
    }

    #[test]
    fn global_slot_is_the_historical_uri_key() {
        // every vector written before per-scope slots is a global record's slot already
        let uri = "daimon://resources/workspace/runbook/shared-runbook";
        assert_eq!(slot_of(uri, ""), pk_for(uri));
        assert_eq!(chunk_pk(uri, "", 0), pk_for(uri));
        assert_ne!(slot_of(uri, "room:a"), slot_of(uri, ""), "a scoped record has its own slot");
        assert_ne!(slot_of(uri, "room:a"), slot_of(uri, "room:b"));
        // injective encoding: the scope's length is part of the key
        assert_ne!(slot_of("xdaimon://u", "ab"), slot_of("daimon://u", "abx"));
        assert_eq!(slot_of(uri, "room:a").len(), SLOT_LEN);
        // a scoped slot is SHA-256 over the length-prefixed key (charset-safe hex, 128 bits)
        assert_eq!(slot_of("daimon://u", "room:a"), "2fa2727abc1c4b14071d892f79584bb0", "sha256(\"s6:room:adaimon://u\")[..16 bytes]");
        assert!(slot_of(uri, "room:a").bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(slot_of(uri, "room:a"), pk_for(&format!("s6:room:a{uri}")), "not the FNV form");
    }

    #[test]
    fn scopes_of_one_uri_hold_separate_slots() {
        let dir = std::env::temp_dir().join(format!("zvecslots-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let uri = "daimon://resources/workspace/runbook/shared-runbook";
        idx.upsert_chunks(uri, "", &[unit(0)]).expect("global");
        idx.upsert_chunks(uri, "room:a", &[unit(100), unit(101)]).expect("room a, two chunks");
        idx.upsert_chunks("daimon://other", "", &[unit(200)]).expect("other");
        // the room save did not overwrite the global vector: each slot answers its own text
        let g = idx.search(&unit(0), 5).unwrap();
        assert_eq!((g[0].uri.as_str(), g[0].slot.clone()), (uri, slot_of(uri, "")));
        assert!(g[0].score > 0.99);
        let r = idx.search(&unit(101), 5).unwrap();
        assert_eq!((r[0].uri.as_str(), r[0].slot.clone()), (uri, slot_of(uri, "room:a")), "a later chunk maps to its slot");
        assert_eq!(r.iter().filter(|h| h.slot == slot_of(uri, "room:a")).count(), 1, "chunks max-pool per slot");
        assert!(r.iter().any(|h| h.slot == slot_of(uri, "")), "the same uri's other slot is a separate hit");
        // removing one slot leaves the other
        idx.remove(uri, "room:a").unwrap();
        let after = idx.search(&unit(101), 5).unwrap();
        assert!(after.iter().all(|h| h.slot != slot_of(uri, "room:a")), "room slot gone: {after:?}");
        assert!(idx.search(&unit(0), 5).unwrap().iter().any(|h| h.slot == slot_of(uri, "") && h.score > 0.99));
    }

    #[test]
    fn upsert_then_search_finds_nearest() {
        let dir = std::env::temp_dir().join(format!("zvectest-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut a = vec![0f32; DIM];
        a[0] = 1.0;
        let mut b = vec![0f32; DIM];
        b[100] = 1.0;
        idx.upsert_chunks("uri-a", "", std::slice::from_ref(&a)).expect("upsert a");
        idx.upsert_chunks("uri-b", "", std::slice::from_ref(&b)).expect("upsert b");
        let hits = pairs(idx.search(&a, 5).expect("search"));
        assert!(
            hits.first().map(|(s, _)| s == "uri-a").unwrap_or(false),
            "nearest to a should be uri-a, got {:?}",
            hits
        );
        // search() returns cosine SIMILARITY (higher = nearer): self-match ~1.0, orthogonal ~0.0
        let by: std::collections::HashMap<_, _> = hits.iter().cloned().collect();
        assert!(by["uri-a"] > 0.9, "self-match similarity should be ~1.0, got {:?}", hits);
        assert!(
            by["uri-a"] > by.get("uri-b").copied().unwrap_or(f32::MIN),
            "aligned vector must outscore the orthogonal one, got {:?}",
            hits
        );
    }

    #[test]
    fn a_flush_keeps_the_open_index_locked() {
        let dir = std::env::temp_dir().join(format!("zvecflushlock-{}-{}", std::process::id(), crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open(create)");
        assert!(ZvecIndex::open(&dir).is_err(), "a second open is refused while the first is open");
        idx.flush().expect("flush");
        assert!(ZvecIndex::open(&dir).is_err(), "still refused after a flush: syncing must not release the lock");
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flushed_and_closed_index_reopens() {
        // flush() syncs the index's files; it must leave the index's locks alone, or the
        // closed collection keeps its lock and the next open in this process is refused.
        let dir = std::env::temp_dir().join(format!("zvecflushreopen-{}-{}", std::process::id(), crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = vec![0f32; DIM];
        a[7] = 1.0;
        {
            let idx = ZvecIndex::open(&dir).expect("open(create)");
            idx.upsert_chunks("uri-f", "", std::slice::from_ref(&a)).expect("upsert");
            idx.flush().expect("flush");
        }
        let idx2 = ZvecIndex::open(&dir).expect("reopen after flush + close");
        assert!(pairs(idx2.search(&a, 5).unwrap()).iter().any(|(u, _)| u == "uri-f"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persists_and_searches_across_reopen() {
        // mirrors real dmem: one process writes, a later process reopens + searches
        let dir = std::env::temp_dir().join(format!("zvecreopen-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = vec![0f32; DIM];
        a[5] = 1.0;
        {
            let idx = ZvecIndex::open(&dir).expect("open(create)");
            idx.upsert_chunks("uri-a", "", std::slice::from_ref(&a)).expect("upsert");
        } // drop / close
        let idx2 = ZvecIndex::open(&dir).expect("open(existing)");
        let hits = pairs(idx2.search(&a, 5).expect("search after reopen"));
        assert!(
            hits.iter().any(|(u, _)| u == "uri-a"),
            "reopened index should still find uri-a, got {:?}",
            hits
        );
    }

    #[test]
    fn chunked_record_maxpools_to_one_hit() {
        // a record with two orthogonal chunk vectors: a query matching EITHER chunk must surface
        // the record (the long-record fix), and it must appear exactly once (max-pool dedup).
        let dir = std::env::temp_dir().join(format!("zvecchunk-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut head = vec![0f32; DIM];
        head[0] = 1.0;
        let mut tail = vec![0f32; DIM];
        tail[100] = 1.0;
        let mut other = vec![0f32; DIM];
        other[200] = 1.0;
        idx.upsert_chunks("uri-doc", "", &[head.clone(), tail.clone()]).expect("upsert chunks");
        idx.upsert_chunks("uri-other", "", &[other]).expect("upsert other");
        // a query matching the TAIL chunk surfaces the record, even though the head is its chunk 0
        let hits = pairs(idx.search(&tail, 5).expect("search tail"));
        assert_eq!(hits.first().map(|(u, _)| u.as_str()), Some("uri-doc"), "tail query should hit the doc, got {hits:?}");
        assert_eq!(hits.iter().filter(|(u, _)| u == "uri-doc").count(), 1, "doc must dedup to one hit, got {hits:?}");
        // and a query matching the HEAD chunk surfaces it too
        let hits_h = pairs(idx.search(&head, 5).expect("search head"));
        assert!(hits_h.iter().any(|(u, _)| u == "uri-doc"), "head query should hit the doc, got {hits_h:?}");
    }

    #[test]
    fn shrinking_chunks_clears_stale() {
        // re-saving a record with fewer chunks must drop the orphaned tail vectors.
        let dir = std::env::temp_dir().join(format!("zvecshrink-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let mut a = vec![0f32; DIM];
        a[0] = 1.0;
        let mut b = vec![0f32; DIM];
        b[100] = 1.0;
        let mut c = vec![0f32; DIM];
        c[200] = 1.0;
        idx.upsert_chunks("uri-x", "", &[a.clone(), b, c.clone()]).expect("upsert 3");
        idx.upsert_chunks("uri-x", "", &[a.clone()]).expect("upsert 1"); // shrink to one chunk
        let hits = pairs(idx.search(&c, 5).expect("search c"));
        // the exact c-chunk would score ~1.0 if it survived; after clearing, uri-x can still appear
        // (only the head chunk remains) but at the orthogonal ~0.0 score, proving the tail is gone.
        let cx = hits.iter().find(|(u, _)| u == "uri-x").map(|(_, s)| *s);
        assert!(cx.unwrap_or(0.0) < 0.5, "stale tail chunk (sim ~1.0) must be cleared; uri-x score for c was {cx:?}");
        let hits_a = pairs(idx.search(&a, 5).expect("search a"));
        assert!(hits_a.iter().any(|(u, _)| u == "uri-x"), "surviving head chunk must still match, got {hits_a:?}");
    }

    #[test]
    fn long_daimon_uri_roundtrips() {
        // the real failure: a daimon:// URI is >64 chars and has : and / - so it can't be the
        // PK (zvec caps PK at 64 bytes). Hashed PK + uri field must round-trip the full URI.
        let dir = std::env::temp_dir().join(format!("zveclonguri-{}", crate::entry::now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = ZvecIndex::open(&dir).expect("open");
        let uri = "daimon://resources/notes/memory/the-postgres-database-server-was-oom-killed-during-migration";
        assert!(uri.len() > 64, "fixture must exceed the 64-byte PK cap");
        let mut v = vec![0f32; DIM];
        v[7] = 1.0;
        idx.upsert_chunks(uri, "", std::slice::from_ref(&v)).expect("upsert long daimon uri");
        let hits = pairs(idx.search(&v, 5).expect("search"));
        assert_eq!(hits.first().map(|(s, _)| s.as_str()), Some(uri), "got {:?}", hits);
    }
}
