//! SA skills as a first-class, queryable memory kind (`Kind::Skill`), projected into Claude Code.
//!
//! The canonical skill lives in the dmem store as a `skill` record whose body IS the full SKILL.md
//! (frontmatter + content); `sync` materializes those records back out to
//! `~/.claude/skills/<name>/SKILL.md` so Claude Code loads them like any plugin skill. Storing the
//! body verbatim keeps `import -> sync -> import` a byte-exact round trip and makes the skill
//! recall-queryable (FTS + vector index over the body) like every other kind.
//!
//! `import` goes through `Memory::open()` so it is mode-aware: in remote-client mode the running
//! `dmem serve` is the sole writer of the store + vector index (no concurrent-writer corruption);
//! in embedded mode it writes the local engine directly. `sync`/`list` read the local tenant db
//! (a concurrent read is safe alongside the server), so no server stop is ever required.

use crate::config;
use crate::entry::{Entry, Kind};
use crate::tools::{LocalMemory, Memory};
use anyhow::{anyhow, Result};
use std::path::Path;

/// Namespace all skill records share.
const SKILLS_NS: &str = "agent/skills";

/// Extract `(name, first-line-of-description)` from a SKILL.md YAML-ish frontmatter block.
/// `name` is required; `description` is best-effort (empty if absent). The body is kept verbatim
/// by the caller, so only these two header keys are read.
fn parse_skill_frontmatter(text: &str) -> Result<(String, String)> {
    let t = text.trim_start();
    let rest = t
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("missing frontmatter (expected a leading ---)"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("unterminated frontmatter (expected a closing ---)"))?;
    let header = &rest[..end];

    let mut name = String::new();
    let mut desc = String::new();
    for line in header.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let val = v.trim().trim_matches('"').trim_matches('\'').trim().to_string();
            match k.trim() {
                "name" => name = val,
                "description" if desc.is_empty() => desc = val,
                _ => {}
            }
        }
    }
    if name.is_empty() {
        return Err(anyhow!("SKILL.md frontmatter is missing a `name`"));
    }
    Ok((name, desc))
}

/// Reject names that could escape the skills root on `sync` (path traversal / separators).
fn safe_skill_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..")
}

/// CORE (testable): import every `<dir>/<name>/SKILL.md` as a `skill` record. Idempotent: the
/// record uri derives from (namespace, kind, slug(name)), so re-import upserts, never duplicates.
/// Writes through `Memory`, so in remote mode the server is the single writer. Returns
/// `(imported, skipped)`.
pub fn import_dir(mem: &Memory, dir: &Path) -> Result<(usize, usize)> {
    let (mut ok, mut skipped) = (0usize, 0usize);
    let mut subdirs: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| anyhow!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    for sub in subdirs {
        let text = match std::fs::read_to_string(sub.join("SKILL.md")) {
            Ok(t) => t,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let name = match parse_skill_frontmatter(&text) {
            Ok((n, _)) => n,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !safe_skill_name(&name) {
            skipped += 1;
            continue;
        }
        mem.import_record(Kind::Skill, SKILLS_NS, &name, &text)?;
        ok += 1;
    }
    Ok((ok, skipped))
}

/// CORE (testable): materialize skill records to `<root>/<name>/SKILL.md` (overwrite). The body is
/// written verbatim. Names failing `safe_skill_name` are skipped. Returns the count written.
pub fn sync_to(records: &[Entry], root: &Path) -> Result<usize> {
    let mut n = 0usize;
    for r in records {
        let name = r.title.trim();
        if !safe_skill_name(name) {
            continue;
        }
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).map_err(|e| anyhow!("create {}: {e}", dir.display()))?;
        std::fs::write(dir.join("SKILL.md"), &r.body).map_err(|e| anyhow!("write {}: {e}", dir.display()))?;
        n += 1;
    }
    Ok(n)
}

/// CORE (testable): `(name, first-line-of-description)` rows for `list`.
pub fn list_rows(records: &[Entry]) -> Vec<(String, String)> {
    records
        .iter()
        .map(|r| {
            let desc = parse_skill_frontmatter(&r.body).map(|(_, d)| d).unwrap_or_default();
            (r.title.clone(), desc)
        })
        .collect()
}

// --- CLI entry points ---

/// `dmem skills import <dir>` — writes through the server (mode-aware), so it is safe while the
/// loopback `dmem serve` is running.
pub fn import(dir: &str) -> Result<()> {
    let mem = Memory::open()?;
    let (ok, skipped) = import_dir(&mem, Path::new(dir))?;
    println!("imported {ok} skill(s), skipped {skipped}");
    Ok(())
}

/// `dmem skills sync` — reads the local tenant store (a safe concurrent read) and projects every
/// skill record to `~/.claude/skills/<name>/SKILL.md`.
pub fn sync() -> Result<()> {
    let mem = LocalMemory::open_tenant(&config::tenant())?;
    let root = config::claude_skills_dir()?;
    let n = sync_to(&mem.skills_all(100_000)?, &root)?;
    println!("synced {n} skill(s) -> {}", root.display());
    Ok(())
}

/// `dmem skills list`
pub fn list() -> Result<()> {
    let mem = LocalMemory::open_tenant(&config::tenant())?;
    let rows = list_rows(&mem.skills_all(100_000)?);
    if rows.is_empty() {
        println!("no skills imported yet (dmem skills import <dir>)");
    }
    for (name, desc) in rows {
        println!("- {name}  {desc}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::now_ms;
    use crate::sqlite::SqliteStore;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn uniq(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("dmskills-{tag}-{}-{}-{}", std::process::id(), now_ms(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A local-backed `Memory` over a fresh temp store (mirrors embedded mode for the test).
    fn mem() -> Memory {
        let store = SqliteStore::open(&uniq("db").join("t.db")).unwrap();
        Memory::Local(LocalMemory::for_test(store))
    }

    /// Write a synthetic (dummy) SKILL.md and return its verbatim text.
    fn write_skill(root: &Path, name: &str, marker: &str) -> String {
        let md = format!(
            "---\nname: {name}\ndescription: \"Synthetic skill {name}; trigger {marker}.\"\n---\n\n# {name}\n\nBody mentions {marker} for recall.\n"
        );
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), &md).unwrap();
        md
    }

    #[test]
    fn parse_extracts_name_and_description() {
        let (n, d) = parse_skill_frontmatter("---\nname: alpha\ndescription: \"does alpha\"\n---\n\nbody\n").unwrap();
        assert_eq!(n, "alpha");
        assert!(d.contains("does alpha"));
        assert!(parse_skill_frontmatter("---\ndescription: no name\n---\n").is_err());
    }

    #[test]
    fn safe_name_guard() {
        assert!(safe_skill_name("design-review"));
        for bad in ["", "../evil", "a/b", "a\\b", "x..y"] {
            assert!(!safe_skill_name(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn import_is_idempotent() {
        let m = mem();
        let src = uniq("src");
        write_skill(&src, "skill-alpha", "alphaword");
        write_skill(&src, "skill-beta", "betaword");
        let (ok, skipped) = import_dir(&m, &src).unwrap();
        assert_eq!((ok, skipped), (2, 0));
        import_dir(&m, &src).unwrap(); // re-import
        assert_eq!(m.as_local().skills_all(100).unwrap().len(), 2, "re-import must upsert, not duplicate");
    }

    #[test]
    fn skill_kind_and_recall_round_trip() {
        let m = mem();
        let src = uniq("src");
        write_skill(&src, "zeta-skill", "zebrawordmarker");
        import_dir(&m, &src).unwrap();
        let all = m.as_local().skills_all(100).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, Kind::Skill);
        assert_eq!(all[0].title, "zeta-skill");
        let hits = m.recall("zebrawordmarker", 5).unwrap();
        assert!(
            hits.iter().any(|e| e.title == "zeta-skill" && e.kind == Kind::Skill),
            "skill must be recall-queryable"
        );
    }

    #[test]
    fn sync_round_trips_into_temp_home() {
        let m = mem();
        let src = uniq("src");
        let body = write_skill(&src, "skill-alpha", "alphaword");
        import_dir(&m, &src).unwrap();
        let recs = m.as_local().skills_all(100).unwrap();
        let out = uniq("home");
        assert_eq!(sync_to(&recs, &out).unwrap(), 1);
        let synced = std::fs::read_to_string(out.join("skill-alpha").join("SKILL.md")).unwrap();
        assert_eq!(synced, body, "synced SKILL.md must be byte-identical to the stored body");
        import_dir(&m, &out).unwrap(); // round trip
        assert_eq!(m.as_local().skills_all(100).unwrap().len(), 1);
    }

    #[test]
    fn list_rows_emits_name_and_desc() {
        let m = mem();
        let src = uniq("src");
        write_skill(&src, "skill-alpha", "alphaword");
        import_dir(&m, &src).unwrap();
        let rows = list_rows(&m.as_local().skills_all(100).unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "skill-alpha");
        assert!(rows[0].1.contains("alphaword"));
    }

    #[test]
    fn sync_rejects_path_traversal_name() {
        let out = uniq("home");
        let evil = Entry::new_now(
            "daimon://x".into(), Kind::Skill, SKILLS_NS.into(), "../evil".into(),
            "body".into(), vec![], 75, "daimon://x".into(),
        );
        assert_eq!(sync_to(&[evil], &out).unwrap(), 0, "traversal name must be skipped");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "nothing written under root");
    }
}
