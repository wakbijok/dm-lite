//! Project awareness: which code project a session is working in, so that saves are stamped
//! with it and recall ranks that project's records first.
//!
//! A project is named by its git `origin` remote, normalized to `host/path` (for example
//! `git.example.org/group/repo`): the same name on every machine and in every checkout of the
//! repository, which a local path is not. A directory with no git remote has no project.
//!
//! The project is a RANKING hint only. It is stamped as a `project:<id>` tag and gives a bounded
//! boost at recall; it never decides who can read a record. Scope stays the only partition.

use std::path::Path;
use std::process::Command;

/// The env override: `off` (or `0`, `none`, `false`) disables project awareness; any other
/// non-empty value names the project outright, skipping git.
pub const PROJECT_ENV: &str = "DM_PROJECT";

/// The request header a remote client sends its project in.
pub const PROJECT_HEADER: &str = "x-dm-project";

/// The tag prefix a saved record carries its project in.
pub const TAG_PREFIX: &str = "project:";

/// The recall multiplier for a record of the current project, applied beside the bounded
/// runtime-signal nudge. Fused (RRF) scores are flat - rank 1 and rank 10 of one channel differ
/// by about 15% - so this orders the current project's records first among hits of the same
/// match strength, while a record both channels found (about twice the score) still outranks a
/// project record only one channel found. Keyword-only recall scores by position, where it can
/// only reorder neighbours deeper in the list.
pub const BOOST: f64 = 1.2;

const MAX_LEN: usize = 200;

/// A project id the engine accepts: printable ASCII without spaces or commas, at most 200
/// bytes. Header-safe and tag-safe; anything else is treated as no project.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_LEN && id.bytes().all(|b| b.is_ascii_graphic() && b != b',')
}

/// Normalize a git remote URL to `host/path`: scheme, credentials, port, a trailing `.git` and
/// trailing slashes are dropped, and the result is lowercased (GitLab and GitHub paths are
/// case-insensitive). Handles `https://user:token@host/path.git`, `ssh://git@host:22/path`,
/// and the scp form `git@host:path.git`. None for a local path or anything unparseable.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host.split_once(':').map_or(host, |(h, _)| h);
        (host, path)
    } else {
        // scp form: [user@]host:path, where the host part has no slash
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || authority.is_empty() {
            return None;
        }
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let id = format!("{host}/{path}").to_ascii_lowercase();
    valid_id(&id).then_some(id)
}

/// What `DM_PROJECT` says: Some(None) = disabled, Some(Some(id)) = named, None = not set.
fn env_override(value: Option<String>) -> Option<Option<String>> {
    let v = value?.trim().to_string();
    if v.is_empty() {
        return None;
    }
    if ["off", "0", "none", "false"].iter().any(|o| v.eq_ignore_ascii_case(o)) {
        return Some(None);
    }
    Some(valid_id(&v).then_some(v))
}

/// The project of `dir`: `DM_PROJECT` first, then the git `origin` remote (or the first remote
/// when there is no `origin`). None when disabled, outside a repository, or without a remote.
/// Never fails: a missing git binary or a broken repository is simply no project.
pub fn detect(dir: &Path) -> Option<String> {
    if let Some(over) = env_override(std::env::var(PROJECT_ENV).ok()) {
        return over;
    }
    detect_git(dir)
}

/// The git half of `detect`: the `origin` remote, else the first remote, normalized.
fn detect_git(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let url = git(&["config", "--get", "remote.origin.url"]).filter(|u| !u.is_empty()).or_else(|| {
        let first = git(&["remote"])?.lines().next()?.trim().to_string();
        git(&["config", "--get", &format!("remote.{first}.url")])
    })?;
    normalize_remote(&url)
}

/// The project of the current working directory (the MCP server and the CLI run in the folder
/// the host agent opened).
pub fn detect_cwd() -> Option<String> {
    std::env::current_dir().ok().and_then(|d| detect(&d))
}

/// The tag a record of `id` carries.
pub fn tag(id: &str) -> String {
    format!("{TAG_PREFIX}{id}")
}

/// Add the project tag unless the record already names a project (a caller-chosen project
/// wins, as an explicit `author:` does).
pub fn stamp(tags: &mut Vec<String>, project: Option<&str>) {
    if let Some(p) = project {
        if !tags.iter().any(|t| t.starts_with(TAG_PREFIX)) {
            tags.push(tag(p));
        }
    }
}

/// The recall multiplier for a record with `tags` when the reader works in `project`.
pub fn boost(tags: &[String], project: Option<&str>) -> f64 {
    match project {
        Some(p) if tags.iter().any(|t| t.strip_prefix(TAG_PREFIX) == Some(p)) => BOOST,
        _ => 1.0,
    }
}

/// Where a file sits: its project, the repository's name (the last segment of the project id),
/// and its path relative to the repository root, with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContext {
    pub project: Option<String>,
    pub repo_name: Option<String>,
    pub rel: String,
}

/// The context of `path` (absolute, or relative to `cwd`): None outside a git work tree.
/// The file need not exist yet (a Write creates it): its directory is resolved instead.
pub fn file_context(path: &Path, cwd: Option<&Path>) -> Option<FileContext> {
    let path = if path.is_absolute() { path.to_path_buf() } else { cwd?.join(path) };
    let dir = path.parent()?;
    let dir = std::fs::canonicalize(dir).ok()?;
    let out = Command::new("git").arg("-C").arg(&dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let root = std::fs::canonicalize(String::from_utf8_lossy(&out.stdout).trim()).ok()?;
    let file = dir.join(path.file_name()?);
    let rel = file.strip_prefix(&root).ok()?.to_str()?.replace('\\', "/");
    if rel.is_empty() {
        return None;
    }
    let project = detect(&dir);
    let repo_name = project.as_deref().and_then(|p| p.rsplit('/').next()).map(str::to_string);
    Some(FileContext { project, repo_name, rel })
}

/// Top-level folder names too common to identify anything on their own: a record mentioning
/// `src/` is not about the file being edited. A deeper folder (`deploy/stack/`) or a distinctive
/// top-level one (`deploy/`) is.
const GENERIC_DIRS: &[&str] = &[
    "src", "lib", "libs", "test", "tests", "spec", "docs", "doc", "scripts", "script", "bin", "app",
    "apps", "pkg", "cmd", "internal", "crates", "packages", "examples", "benches", "assets", "static",
    "public", "config", "vendor", "build", "dist", "target", ".github", "contrib", "tools",
];

/// Whether `text` names `needle` as a path: present, and not glued to a longer name on its left
/// (`mysrc/x` does not mention `src/x`). Case-insensitive.
fn names_path(text: &str, needle: &str) -> bool {
    let (text, needle) = (text.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let mut from = 0;
    while let Some(i) = text[from..].find(&needle) {
        let at = from + i;
        let left_ok = text[..at].chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'));
        if left_ok {
            return true;
        }
        from = at + needle.len().max(1);
    }
    false
}

/// Whether record text (title, body and tags together) is about the file at `rel`: it names the
/// path itself, or a folder of it that is not generic (see `GENERIC_DIRS`), written with its
/// trailing slash.
pub fn mentions_file(text: &str, rel: &str) -> bool {
    if names_path(text, rel) {
        return true;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    (1..parts.len()).any(|n| {
        let dir = parts[..n].join("/");
        let distinctive = n >= 2 || !GENERIC_DIRS.contains(&parts[0].to_ascii_lowercase().as_str());
        distinctive && names_path(text, &format!("{dir}/"))
    })
}

/// The keyword query that finds records about the file at `rel`: its distinctive parts only (the
/// file name without a short extension, the non-generic folders), so generic terms like `src` or
/// `rs` do not flood the pool. Not the repository's name either: every record of the project
/// names it, and `belongs` checks it afterwards, as `mentions_file` checks the exact path.
pub fn file_query(ctx: &FileContext) -> String {
    let parts: Vec<&str> = ctx.rel.split('/').collect();
    let mut terms: Vec<String> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let part = if last {
            match part.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() && ext.len() <= 4 => stem,
                _ => part,
            }
        } else {
            part
        };
        if !last && GENERIC_DIRS.contains(&part.to_ascii_lowercase().as_str()) {
            continue;
        }
        terms.push(part.to_string());
    }
    terms.join(" ")
}

/// Whether a record with `tags` and `text` belongs to the file's project: it carries that
/// project's tag; or it carries no project tag (saved before project awareness) and its text
/// names the repository.
pub fn belongs(tags: &[String], text: &str, ctx: &FileContext) -> bool {
    let tagged: Vec<&str> = tags.iter().filter_map(|t| t.strip_prefix(TAG_PREFIX)).collect();
    if !tagged.is_empty() {
        return ctx.project.as_deref().is_some_and(|p| tagged.contains(&p));
    }
    ctx.repo_name.as_deref().is_some_and(|r| r.len() >= 3 && names_path(text, r))
}

/// Where a repository's work stands, from local git only (no fetch): what a session should
/// know before it starts, because a development checkout can be lost and GitLab is the copy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkState {
    pub repo: String,
    pub branch: String,
    /// The branch's upstream, if it has one.
    pub upstream: Option<String>,
    /// Commits on the branch that its upstream does not have (as of the last fetch).
    pub ahead: usize,
    /// Commits on the upstream the branch does not have (as of the last fetch).
    pub behind: usize,
    /// Other local branches with commits that no remote branch has: (name, commit count).
    pub local_only: Vec<(String, usize)>,
    /// Changed, staged or untracked files.
    pub dirty: usize,
}

/// The work state of the repository holding `dir`; None outside a git work tree.
pub fn work_state(dir: &Path) -> Option<WorkState> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    let root = git(&["rev-parse", "--show-toplevel"])?;
    let repo = detect(dir)
        .and_then(|p| p.rsplit('/').next().map(str::to_string))
        .or_else(|| Path::new(&root).file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_default();
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "(no commits)".into());
    let upstream = git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).filter(|u| !u.is_empty());
    let (behind, ahead) = match &upstream {
        Some(_) => git(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .and_then(|s| {
                let mut it = s.split_whitespace().filter_map(|n| n.parse::<usize>().ok());
                Some((it.next()?, it.next()?))
            })
            .unwrap_or((0, 0)),
        // No upstream: every commit not on any remote is unpushed.
        None => (0, git(&["rev-list", "--count", "HEAD", "--not", "--remotes"]).and_then(|n| n.parse().ok()).unwrap_or(0)),
    };
    let mut local_only = Vec::new();
    for b in git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap_or_default().lines() {
        if b == branch {
            continue;
        }
        let n: usize = git(&["rev-list", "--count", b, "--not", "--remotes"]).and_then(|n| n.parse().ok()).unwrap_or(0);
        if n > 0 {
            local_only.push((b.to_string(), n));
        }
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=normal"]).map(|s| s.lines().filter(|l| !l.is_empty()).count()).unwrap_or(0);
    Some(WorkState { repo, branch, upstream, ahead, behind, local_only, dirty })
}

impl WorkState {
    /// Nothing is at risk: no unpushed commit, no local-only branch, no uncommitted file.
    pub fn is_safe(&self) -> bool {
        self.ahead == 0 && self.local_only.is_empty() && self.dirty == 0
    }

    /// One line for the session-start block.
    pub fn line(&self) -> String {
        let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let mut parts: Vec<String> = Vec::new();
        match &self.upstream {
            Some(u) if self.ahead > 0 => parts.push(format!("{} not pushed to {u}", plural(self.ahead, "commit", "commits"))),
            Some(u) if self.behind == 0 => parts.push(format!("in sync with {u}")),
            Some(_) => {}
            None if self.ahead > 0 => parts.push(format!("no upstream, {} on no remote", plural(self.ahead, "commit", "commits"))),
            None => parts.push("no upstream".into()),
        }
        if self.behind > 0 {
            parts.push(format!("{} behind (as of the last fetch)", plural(self.behind, "commit", "commits")));
        }
        if !self.local_only.is_empty() {
            let list: Vec<String> = self.local_only.iter().take(5).map(|(b, n)| format!("{b} ({n})")).collect();
            let more = if self.local_only.len() > 5 { format!(", +{} more", self.local_only.len() - 5) } else { String::new() };
            parts.push(format!("{} with unpushed commits: {}{more}", plural(self.local_only.len(), "local branch", "local branches"), list.join(", ")));
        }
        if self.dirty > 0 {
            parts.push(plural(self.dirty, "uncommitted file", "uncommitted files"));
        }
        let lead = if self.is_safe() { "" } else { "push or commit what matters: " };
        format!("[work state] {} on {}: {lead}{}", self.repo, self.branch, parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_remote_forms() {
        let want = Some("git.example.org/group/repo".to_string());
        for url in [
            "https://git.example.org/group/repo.git",
            "https://user:glpat-secret@git.example.org/group/repo.git",
            "https://oauth2:tok@git.example.org:443/group/repo/",
            "ssh://git@git.example.org:2222/group/repo.git",
            "git@git.example.org:group/repo.git",
            "git@Git.Example.org:Group/Repo",
            "  https://git.example.org/group/repo  ",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
        assert_eq!(normalize_remote("https://github.com/owner/name"), Some("github.com/owner/name".into()));
        assert_eq!(normalize_remote("https://git.example.org/a/b/c.git"), Some("git.example.org/a/b/c".into()));
    }

    #[test]
    fn credentials_never_reach_the_id() {
        let id = normalize_remote("https://arif:supersecret@git.example.org/g/r.git").unwrap();
        assert!(!id.contains("supersecret") && !id.contains("arif"), "{id}");
    }

    #[test]
    fn local_paths_and_junk_have_no_project() {
        for url in ["/srv/git/repo.git", "./repo", "file:///srv/git/repo.git", "", "https://host", "https://host/", ":path", "host:"] {
            assert_eq!(normalize_remote(url), None, "{url:?}");
        }
    }

    #[test]
    fn env_override_disables_or_names() {
        assert_eq!(env_override(None), None);
        assert_eq!(env_override(Some("  ".into())), None);
        for off in ["off", "OFF", "0", "none", "false"] {
            assert_eq!(env_override(Some(off.into())), Some(None), "{off}");
        }
        assert_eq!(env_override(Some("my/project".into())), Some(Some("my/project".into())));
        assert_eq!(env_override(Some("has space".into())), Some(None));
        assert_eq!(env_override(Some("a,b".into())), Some(None));
    }

    #[test]
    fn stamp_adds_once_and_respects_an_explicit_project() {
        let mut t = vec!["decision".to_string()];
        stamp(&mut t, Some("h/a"));
        stamp(&mut t, Some("h/a"));
        assert_eq!(t, vec!["decision".to_string(), "project:h/a".to_string()]);
        let mut explicit = vec!["project:h/other".to_string()];
        stamp(&mut explicit, Some("h/a"));
        assert_eq!(explicit, vec!["project:h/other".to_string()]);
        let mut none = vec![];
        stamp(&mut none, None);
        assert!(none.is_empty());
    }

    #[test]
    fn boost_only_for_the_current_project() {
        let tags = vec!["project:h/a".to_string()];
        assert_eq!(boost(&tags, Some("h/a")), BOOST);
        assert_eq!(boost(&tags, Some("h/ab")), 1.0);
        assert_eq!(boost(&tags, Some("h")), 1.0);
        assert_eq!(boost(&tags, None), 1.0);
        assert_eq!(boost(&[], Some("h/a")), 1.0);
    }

    #[test]
    fn a_record_mentions_the_file_or_a_distinctive_folder() {
        let rel = "deploy/stack/install.sh";
        assert!(mentions_file("never edit deploy/stack/install.sh while it runs", rel));
        assert!(mentions_file("bash reads scripts as it runs: never touch deploy/ during a deploy", rel));
        assert!(mentions_file("the deploy/stack/ scripts are sourced", rel));
        assert!(mentions_file("NEVER EDIT DEPLOY/STACK/INSTALL.SH", rel), "case-insensitive");
        assert!(!mentions_file("the deploy step failed", rel), "a word is not a folder");
        assert!(!mentions_file("redeploy/stack/install.sh is elsewhere", rel), "glued to a longer name");
        let rel = "src/hooks.rs";
        assert!(mentions_file("format_output in src/hooks.rs:17 shapes the payload", rel));
        assert!(!mentions_file("everything under src/ is Rust", rel), "a generic top-level folder is not enough");
        assert!(!mentions_file("see src/render.rs", rel));
        assert!(!mentions_file("mysrc/hooks.rs", rel));
    }

    #[test]
    fn a_record_belongs_by_tag_or_by_naming_the_repository() {
        let ctx = FileContext { project: Some("h/team/dm-lite".into()), repo_name: Some("dm-lite".into()), rel: "src/x.rs".into() };
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(belongs(&t(&["project:h/team/dm-lite"]), "", &ctx));
        assert!(!belongs(&t(&["project:h/team/other"]), "mentions dm-lite anyway", &ctx), "another project's tag wins");
        assert!(belongs(&t(&["decision"]), "dm-lite release note", &ctx), "an untagged record naming the repo");
        assert!(!belongs(&t(&[]), "a note about some other repo", &ctx));
        let no_project = FileContext { project: None, repo_name: None, rel: "x".into() };
        assert!(!belongs(&t(&["project:h/team/dm-lite"]), "dm-lite", &no_project));
    }

    #[test]
    fn the_file_query_keeps_the_distinctive_parts() {
        let ctx = |rel: &str| FileContext { project: Some("h/t/dm-lite".into()), repo_name: Some("dm-lite".into()), rel: rel.into() };
        assert_eq!(file_query(&ctx("src/hooks.rs")), "hooks");
        assert_eq!(file_query(&ctx("deploy/stack/install.sh")), "deploy stack install");
        assert_eq!(file_query(&ctx("Makefile")), "Makefile");
        assert_eq!(file_query(&ctx(".gitlab-ci.yml")), ".gitlab-ci");
    }

    #[test]
    fn file_context_resolves_root_relative_paths_and_new_files() {
        let base = std::env::temp_dir().join(format!("dmfilectx-{}-{}", std::process::id(), crate::entry::now_ms()));
        let repo = base.join("Repo");
        std::fs::create_dir_all(repo.join("deploy/stack")).unwrap();
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).output().map(|o| o.status.success()).unwrap_or(false);
        if git(&["init", "-q"]) && git(&["remote", "add", "origin", "git@h.example:team/repo.git"]) && std::env::var(PROJECT_ENV).is_err() {
            let ctx = file_context(&repo.join("deploy/stack/new.sh"), None).expect("a file that does not exist yet still has a context");
            assert_eq!(ctx.rel, "deploy/stack/new.sh");
            assert_eq!(ctx.project.as_deref(), Some("h.example/team/repo"));
            assert_eq!(ctx.repo_name.as_deref(), Some("repo"));
            let rel = file_context(Path::new("deploy/stack/new.sh"), Some(&repo)).unwrap();
            assert_eq!(rel.rel, "deploy/stack/new.sh", "relative to the session's cwd");
            assert_eq!(file_context(&base.join("loose.txt"), None), None, "outside a work tree");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_work_state_line_names_what_is_at_risk() {
        let mut w = WorkState { repo: "dm-lite".into(), branch: "main".into(), upstream: Some("origin/main".into()), ..Default::default() };
        assert_eq!(w.line(), "[work state] dm-lite on main: in sync with origin/main");
        w.ahead = 2;
        w.dirty = 1;
        w.local_only = vec![("feat/x".into(), 3)];
        assert_eq!(
            w.line(),
            "[work state] dm-lite on main: push or commit what matters: 2 commits not pushed to origin/main; 1 local branch with unpushed commits: feat/x (3); 1 uncommitted file"
        );
        let w = WorkState { repo: "r".into(), branch: "feat/y".into(), upstream: None, ahead: 4, ..Default::default() };
        assert_eq!(w.line(), "[work state] r on feat/y: push or commit what matters: no upstream, 4 commits on no remote");
        let w = WorkState { repo: "r".into(), branch: "main".into(), upstream: Some("origin/main".into()), behind: 3, ..Default::default() };
        assert!(w.is_safe());
        assert_eq!(w.line(), "[work state] r on main: 3 commits behind (as of the last fetch)");
    }

    #[test]
    fn work_state_reads_a_real_repository() {
        let base = std::env::temp_dir().join(format!("dmwork-{}-{}", std::process::id(), crate::entry::now_ms()));
        let (remote, repo) = (base.join("remote.git"), base.join("repo"));
        std::fs::create_dir_all(&repo).unwrap();
        let g = |dir: &Path, args: &[&str]| Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output().map(|o| o.status.success()).unwrap_or(false);
        let ok = g(&base, &["init", "-q", "--bare", remote.to_str().unwrap()])
            && g(&repo, &["init", "-q", "-b", "main"])
            && g(&repo, &["remote", "add", "origin", remote.to_str().unwrap()])
            && g(&repo, &["commit", "-q", "--allow-empty", "-m", "one"])
            && g(&repo, &["push", "-q", "-u", "origin", "main"]);
        if ok {
            assert!(work_state(&repo).unwrap().is_safe());
            assert!(g(&repo, &["commit", "-q", "--allow-empty", "-m", "two"]));
            assert!(g(&repo, &["branch", "side"]));
            std::fs::write(repo.join("new.txt"), "x").unwrap();
            let w = work_state(&repo).unwrap();
            assert_eq!((w.branch.as_str(), w.ahead, w.dirty), ("main", 1, 1), "{w:?}");
            assert_eq!(w.local_only, vec![("side".to_string(), 1)]);
            assert_eq!(w.upstream.as_deref(), Some("origin/main"));
            assert_eq!(w.repo, "repo", "no normalizable remote: the folder name");
        }
        assert_eq!(work_state(&base), None, "outside a work tree");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn detects_a_real_repository_and_none_outside_one() {
        let base = std::env::temp_dir().join(format!("dmproject-{}-{}", std::process::id(), crate::entry::now_ms()));
        let (repo, plain) = (base.join("repo"), base.join("plain"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let run = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&repo).args(args).output().map(|o| o.status.success()).unwrap_or(false)
        };
        if !run(&["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git on this host: detection is best-effort by design
        }
        assert!(run(&["remote", "add", "upstream", "https://h.example/x/first.git"]));
        assert_eq!(detect_git(&repo), Some("h.example/x/first".into()));
        assert!(run(&["remote", "add", "origin", "git@h.example:x/main.git"]));
        assert_eq!(detect_git(&repo), Some("h.example/x/main".into()));
        assert_eq!(detect_git(&plain), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
