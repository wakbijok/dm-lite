//! dmem - daimon-memory v2: typed memory with hybrid recall; client/server in one binary. A
//! `[server]` block points the binary at a `dmem serve` (local loopback or remote); without one
//! it falls back to the deprecated embedded mode (a single local tenant).

mod bootstrap;
#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
mod migrate;
mod config;
mod embedder;
mod entry;
mod hooks;
mod mcp;
mod render;
mod sqlite;
mod store;
mod tools;
#[cfg(feature = "server")]
mod iam;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod service;
#[cfg(feature = "wizard")]
mod setup;
#[cfg(feature = "self-update")]
mod upgrade;
#[cfg(feature = "zvec")]
mod zvec_index;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tools::Memory;

/// The default template set, embedded so `dmem template export` is self-contained.
const TPL_FILES: &[(&str, &str)] = &[
    ("README.md", include_str!("../templates/README.md")),
    ("persona.md", include_str!("../templates/persona.md")),
    ("save-discipline.md", include_str!("../templates/save-discipline.md")),
    ("behavioral-discipline.md", include_str!("../templates/behavioral-discipline.md")),
];

#[derive(Parser)]
#[command(name = "dmem", version, about = "daimon-memory v2: typed memory with hybrid recall; client/server in one binary")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum Cmd {
    /// Interactive first-run setup: detect agents, wire hooks, seed memory. Needs --features wizard.
    #[cfg(feature = "wizard")]
    Setup,
    /// Wire (or with --remove, unwire) dmem into your agents (hooks for Devin/Claude Code/Codex/
    /// Hermes; an MCP entry for hook-less Claude Desktop).
    Bootstrap {
        #[arg(long)]
        devin: bool,
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        codex: bool,
        #[arg(long)]
        hermes: bool,
        /// Claude Desktop (MCP only; no hooks). Adds an mcpServers.dmem entry.
        #[arg(long = "claude-desktop")]
        claude_desktop: bool,
        #[arg(long)]
        all: bool,
        /// remove dm's hooks instead of adding them
        #[arg(long)]
        remove: bool,
    },
    /// Lifecycle hook handlers (called by the agent; emit context on stdout).
    Hook {
        #[command(subcommand)]
        event: HookCmd,
        /// emit Hermes-shape output ({"context": ...}) and read Hermes hook-input fields
        #[arg(long, global = true)]
        hermes: bool,
    },
    /// Save a typed Decision.
    LogDecision {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        context: String,
        #[arg(long)]
        decision: String,
        #[arg(long, default_value = "")]
        rationale: String,
        #[arg(long, default_value = "resources/notes")]
        namespace: String,
    },
    /// Save a typed Lesson.
    LogLesson {
        #[arg(long)]
        title: String,
        #[arg(long)]
        lesson: String,
        #[arg(long, default_value = "agent/lessons")]
        namespace: String,
    },
    /// Save a typed Incident.
    LogIncident {
        #[arg(long)]
        title: String,
        #[arg(long)]
        impact: String,
        #[arg(long, default_value = "")]
        resolution: String,
        #[arg(long, default_value = "resources/incidents")]
        namespace: String,
    },
    /// Save a free-form memory.
    Remember {
        text: String,
        #[arg(long, default_value = "resources/notes")]
        namespace: String,
        /// valid-time start epoch-ms (default now); backdate when a fact became true
        #[arg(long = "valid-from")]
        valid_from: Option<i64>,
        /// valid-time end epoch-ms (default open / still true)
        #[arg(long = "valid-to")]
        valid_to: Option<i64>,
    },
    /// Invalidate a record from a valid time onward (keeps history; distinct from forget).
    Invalidate {
        uri: String,
        /// epoch-ms from which the fact stops being true
        #[arg(long = "valid-to")]
        valid_to: i64,
    },
    /// Recall memory for a query (human-readable).
    Recall {
        query: Vec<String>,
        #[arg(long, default_value_t = 6)]
        limit: usize,
        /// Bitemporal: recall the store AS OF this system-time epoch-ms (what we believed then).
        #[arg(long = "as-of", visible_alias = "as_of")]
        as_of: Option<i64>,
        /// Bitemporal: recall facts VALID AT this epoch-ms (what was true then); defaults to as-of.
        #[arg(long = "valid-at", visible_alias = "valid_at")]
        valid_at: Option<i64>,
        /// Graph: also pull each hit's neighborhood within this many hops (0 = off).
        #[arg(long, default_value_t = 0)]
        expand: usize,
    },
    /// Show recent high-importance memory.
    Recent {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Show the full version lineage of a record (append-only history).
    History {
        uri: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Retract a record by uri: drop it from recall, keep its history.
    Forget {
        uri: String,
    },
    /// Link two records (the graph layer): from -[rel]-> to.
    Link {
        from: String,
        to: String,
        #[arg(long, default_value = "links")]
        rel: String,
    },
    /// Remove an edge: from -[rel]-> to.
    Unlink {
        from: String,
        to: String,
        #[arg(long, default_value = "links")]
        rel: String,
    },
    /// Show the edges touching a record (its graph connections).
    Links {
        uri: String,
    },
    /// Rebuild edges from the [[name]] references in every record body (batch).
    ReindexLinks,
    /// Save a typed Reminder.
    AddReminder {
        #[arg(long)]
        title: String,
        #[arg(long)]
        text: String,
        #[arg(long, default_value = "agent/reminders")]
        namespace: String,
    },
    /// Save a typed Runbook (a procedure worth repeating).
    LogRunbook {
        #[arg(long)]
        title: String,
        #[arg(long)]
        steps: String,
        #[arg(long, default_value = "resources/runbooks")]
        namespace: String,
    },
    /// Save a typed Convention (a standing rule).
    LogConvention {
        #[arg(long)]
        title: String,
        #[arg(long)]
        rule: String,
        #[arg(long, default_value = "resources/conventions")]
        namespace: String,
    },
    /// Import a template/markdown file (or a directory of them) as memory records.
    Import {
        path: String,
    },
    /// Migrate a daimon-memory v1 export (JSONL) into dm-lite. Needs --features client.
    #[cfg(feature = "client")]
    Migrate {
        /// JSONL export file (or - for stdin)
        #[arg(long)]
        file: Option<String>,
        /// v1 server URL to pull GET /admin/export from
        #[arg(long)]
        url: Option<String>,
        /// v1 admin token (with --url)
        #[arg(long)]
        token: Option<String>,
        /// accept a self-signed/invalid TLS cert from the v1 server (with --url)
        #[arg(long)]
        insecure: bool,
        /// trust a specific CA/self-signed cert (PEM path) for the v1 server (with --url)
        #[arg(long = "ca-cert")]
        ca_cert: Option<String>,
    },
    /// Template helpers (export the bundled defaults to edit).
    #[command(subcommand)]
    Template(TemplateCmd),
    /// Show store + wiring status.
    Status,
    /// Run as an MCP stdio server (recall + typed save tools for MCP-aware agents).
    Mcp,
    /// Run the network API server (multi-token bearer -> tenant). Needs --features server.
    #[cfg(feature = "server")]
    Serve {
        #[arg(long, default_value = "127.0.0.1:8077")]
        addr: String,
        /// TLS certificate (PEM); pair with --tls-key for HTTPS
        #[arg(long = "tls-cert")]
        tls_cert: Option<String>,
        /// TLS private key (PEM)
        #[arg(long = "tls-key")]
        tls_key: Option<String>,
        /// generate a self-signed cert for HTTPS (saved under the data dir)
        #[arg(long = "tls-generate")]
        tls_generate: bool,
    },
    /// Manage the local `dmem serve` daemon as an OS service (launchd/systemd). Needs --features server.
    #[cfg(feature = "server")]
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Update dmem in place from GitHub Releases. Needs --features self-update.
    #[cfg(feature = "self-update")]
    Upgrade {
        /// include pre-releases (rc/beta), not just stable
        #[arg(long)]
        pre: bool,
    },
    /// Connect this dmem to a remote server (writes the [server] config). Needs --features client.
    #[cfg(feature = "client")]
    Login {
        url: String,
        token: String,
        #[arg(long)]
        insecure: bool,
        #[arg(long = "ca-cert")]
        ca_cert: Option<String>,
    },
    /// Disconnect from the remote server (clears [server]). Needs --features client.
    #[cfg(feature = "client")]
    Logout,
    /// Admin (root token): manage tenants on a server. Needs --features client.
    #[cfg(feature = "client")]
    #[command(subcommand)]
    Admin(AdminCmd),
}

#[cfg(feature = "server")]
#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum ServiceCmd {
    /// Install + start `dmem serve` as a login service (writes the launchd/systemd unit + [server] config)
    Install {
        #[arg(long, default_value = "127.0.0.1:8077")]
        addr: String,
    },
    /// Stop + remove the service unit
    Uninstall,
    /// Start the service
    Start,
    /// Stop the service (reclaims the model RAM)
    Stop,
    /// Restart the service
    Restart,
    /// Show whether the service is running
    Status,
}

#[cfg(feature = "client")]
#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum AdminCmd {
    /// Create a tenant and issue a one-time token.
    Add {
        tenant: String,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long, default_value = "")]
        display: String,
    },
    /// List tenants and live token counts.
    List,
    /// Revoke a token (by value) or all of a tenant's tokens.
    Revoke { target: String },
    /// Suspend a tenant and revoke its tokens.
    Rm { tenant: String },
}

#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum TemplateCmd {
    /// Write the bundled default templates to a directory to edit.
    Export { dir: String },
}

#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum HookCmd {
    /// SessionStart: inject persona/protocol + recent context.
    SessionStart,
    /// UserPromptSubmit: recall for the prompt (read from stdin JSON or arg).
    UserPromptSubmit { prompt: Vec<String> },
    /// SessionEnd: nudge to capture uncaptured decisions before the session ends.
    SessionEnd,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dm: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        #[cfg(feature = "wizard")]
        Cmd::Setup => setup::run(),
        Cmd::Bootstrap { devin, claude, codex, hermes, claude_desktop, all, remove } => {
            bootstrap::run_mode(devin || all, claude || all, codex || all, hermes || all, claude_desktop || all, remove)
        }
        Cmd::Hook { event, hermes } => match event {
            HookCmd::SessionStart => hooks::session_start(hermes),
            HookCmd::UserPromptSubmit { prompt } => {
                let arg = if prompt.is_empty() { None } else { Some(prompt.join(" ")) };
                hooks::user_prompt_submit(arg, hermes)
            }
            HookCmd::SessionEnd => hooks::session_end(),
        },
        Cmd::LogDecision { title, context, decision, rationale, namespace } => {
            let uri = Memory::open()?.log_decision(&title, &context, &decision, &rationale, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::LogLesson { title, lesson, namespace } => {
            let uri = Memory::open()?.log_lesson(&title, &lesson, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::LogIncident { title, impact, resolution, namespace } => {
            let uri = Memory::open()?.log_incident(&title, &impact, &resolution, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::Remember { text, namespace, valid_from, valid_to } => {
            let uri = Memory::open()?.remember(&text, &namespace, valid_from, valid_to)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::Invalidate { uri, valid_to } => {
            let n = Memory::open()?.invalidate(&uri, valid_to)?;
            if n == 0 {
                println!("nothing to invalidate for {}", uri);
            } else {
                println!("invalidated {} ({} segment{} ended at {})", uri, n, if n == 1 { "" } else { "s" }, valid_to);
            }
            Ok(())
        }
        Cmd::Recall { query, limit, as_of, valid_at, expand } => {
            let q = query.join(" ");
            let m = Memory::open()?;
            let hits = if as_of.is_some() || valid_at.is_some() {
                let now = entry::now_ms();
                let sys = as_of.unwrap_or(now);
                let val = valid_at.or(as_of).unwrap_or(now);
                m.recall_as_of(&q, limit, sys, val)?
            } else if expand > 0 {
                m.recall_expanded(&q, limit, expand)?
            } else {
                m.recall(&q, limit)?
            };
            if hits.is_empty() {
                println!("(no matches for '{}')", q);
            } else {
                for e in hits {
                    println!("- ({}) {}  [{}]", e.kind.as_str(), e.title, e.uri);
                }
            }
            Ok(())
        }
        Cmd::Link { from, to, rel } => {
            Memory::open()?.link(&from, &to, &rel)?;
            println!("linked {} -[{}]-> {}", from, rel, to);
            Ok(())
        }
        Cmd::Unlink { from, to, rel } => {
            let n = Memory::open()?.unlink(&from, &to, &rel)?;
            if n == 0 {
                println!("no such edge: {} -[{}]-> {}", from, rel, to);
            } else {
                println!("unlinked {} -[{}]-> {}", from, rel, to);
            }
            Ok(())
        }
        Cmd::Links { uri } => {
            let edges = Memory::open()?.edges_of(&uri)?;
            if edges.is_empty() {
                println!("(no edges for {})", uri);
            } else {
                for e in edges {
                    if e.from_uri == uri {
                        println!("-> [{}] {}", e.rel, e.to_uri);
                    } else {
                        println!("<- [{}] {}", e.rel, e.from_uri);
                    }
                }
            }
            Ok(())
        }
        Cmd::ReindexLinks => {
            let n = Memory::open()?.reindex_links()?;
            println!("reindexed: {} [[link]] reference{} linked", n, if n == 1 { "" } else { "s" });
            Ok(())
        }
        Cmd::Recent { limit } => {
            for e in Memory::open()?.recent(limit)? {
                println!("- ({}) {}  [{}]", e.kind.as_str(), e.title, e.uri);
            }
            Ok(())
        }
        Cmd::History { uri, limit } => {
            let versions = Memory::open()?.history(&uri, limit)?;
            if versions.is_empty() {
                println!("(no record for '{}')", uri);
            } else {
                for (i, e) in versions.iter().enumerate() {
                    let sys_to = e.system_to_ms.map(|t| t.to_string()).unwrap_or_else(|| "current".into());
                    let val_to = e.valid_to_ms.map(|t| t.to_string()).unwrap_or_else(|| "open".into());
                    println!(
                        "- [v{}] ({}) {}  sys[{}..{}] valid[{}..{}]",
                        versions.len() - i, e.kind.as_str(), e.title,
                        e.system_from_ms, sys_to, e.valid_from_ms, val_to
                    );
                }
            }
            Ok(())
        }
        Cmd::AddReminder { title, text, namespace } => {
            let uri = Memory::open()?.add_reminder(&title, &text, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::LogRunbook { title, steps, namespace } => {
            let uri = Memory::open()?.log_runbook(&title, &steps, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::LogConvention { title, rule, namespace } => {
            let uri = Memory::open()?.log_convention(&title, &rule, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::Forget { uri } => {
            let n = Memory::open()?.forget(&uri)?;
            if n == 0 {
                println!("nothing to forget for {}", uri);
            } else {
                println!("forgot {} ({} version{} retired, history kept)", uri, n, if n == 1 { "" } else { "s" });
            }
            Ok(())
        }
        Cmd::Import { path } => {
            let m = Memory::open()?;
            let p = std::path::Path::new(&path);
            let (root, mut files) = if p.is_dir() {
                let mut v = Vec::new();
                collect_md(p, &mut v)?;
                (p.to_path_buf(), v)
            } else {
                (p.parent().map(|x| x.to_path_buf()).unwrap_or_default(), vec![p.to_path_buf()])
            };
            files.sort();
            if files.is_empty() {
                println!("no .md files at {}", p.display());
                return Ok(());
            }
            let (mut ok, mut skipped) = (0usize, 0usize);
            for f in &files {
                let text = match std::fs::read_to_string(f) {
                    Ok(t) => t,
                    Err(_) => { skipped += 1; continue; }
                };
                // our templates carry frontmatter; arbitrary markdown (an Obsidian vault) does
                // not, so fall back to inferring kind/namespace/title from the file.
                let (kind, ns, title, body) = match entry::parse_frontmatter(&text) {
                    Ok(r) => r,
                    Err(_) => infer_record(&root, f, &text),
                };
                if title.trim().is_empty() {
                    skipped += 1;
                    continue;
                }
                match m.import_record(kind, &ns, &title, &body) {
                    Ok(uri) => {
                        ok += 1;
                        println!("imported {} ({}) -> {}", f.file_name().and_then(|n| n.to_str()).unwrap_or("?"), kind.as_str(), uri);
                    }
                    Err(e) => { skipped += 1; eprintln!("skip {}: {e}", f.display()); }
                }
            }
            println!("imported {ok}, skipped {skipped}");
            Ok(())
        }
        #[cfg(feature = "client")]
        Cmd::Migrate { file, url, token, insecure, ca_cert } => migrate::run(file, url, token, insecure, ca_cert),
        Cmd::Template(TemplateCmd::Export { dir }) => {
            let d = std::path::Path::new(&dir);
            std::fs::create_dir_all(d)?;
            for (name, content) in TPL_FILES {
                std::fs::write(d.join(name), content)?;
            }
            println!("wrote {} templates to {}", TPL_FILES.len(), d.display());
            println!("edit persona.md (fill the <PLACEHOLDERS>), then:  dmem import {}", d.display());
            Ok(())
        }
        Cmd::Status => status(),
        Cmd::Mcp => mcp::serve(),
        #[cfg(feature = "server")]
        Cmd::Serve { addr, tls_cert, tls_key, tls_generate } => server::run_blocking(
            &addr,
            server::TlsOpts { cert: tls_cert, key: tls_key, generate: tls_generate },
        ),
        #[cfg(feature = "server")]
        Cmd::Service(s) => match s {
            ServiceCmd::Install { addr } => service::install(&addr),
            ServiceCmd::Uninstall => service::uninstall(),
            ServiceCmd::Start => service::start(),
            ServiceCmd::Stop => service::stop(),
            ServiceCmd::Restart => service::restart(),
            ServiceCmd::Status => service::status(),
        },
        #[cfg(feature = "self-update")]
        Cmd::Upgrade { pre } => upgrade::run(pre),
        #[cfg(feature = "client")]
        Cmd::Login { url, token, insecure, ca_cert } => client::login(&url, &token, insecure, ca_cert),
        #[cfg(feature = "client")]
        Cmd::Logout => client::logout(),
        #[cfg(feature = "client")]
        Cmd::Admin(a) => {
            let link = config::server_link().ok_or_else(|| {
                anyhow::anyhow!("no [server] in config; run `dmem login <url> <admin-token>` first")
            })?;
            let rc = client::RemoteClient::new(link)?;
            match a {
                AdminCmd::Add { tenant, label, display } => {
                    let (t, tok) = rc.admin_add(&tenant, &label, &display)?;
                    println!("created tenant '{t}'. one-time token (save it now, shown once):");
                    println!("    {tok}");
                    println!("the user runs:  dmem login {} {tok}", link.url);
                }
                AdminCmd::List => {
                    if let Some(arr) = rc.admin_list()?.as_array() {
                        for row in arr {
                            println!(
                                "- {:<20} {:<10} {} token(s)",
                                row.get("tenant").and_then(|x| x.as_str()).unwrap_or("?"),
                                row.get("status").and_then(|x| x.as_str()).unwrap_or("?"),
                                row.get("tokens").and_then(|x| x.as_i64()).unwrap_or(0)
                            );
                        }
                    }
                }
                AdminCmd::Revoke { target } => println!("revoked {} token(s)", rc.admin_revoke(&target)?),
                AdminCmd::Rm { tenant } => {
                    rc.admin_rm(&tenant)?;
                    println!("removed tenant {tenant}");
                }
            }
            Ok(())
        }
    }
}

/// Recursively collect `.md` files under `dir`, skipping hidden dirs (e.g. `.obsidian`).
fn collect_md(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            let hidden = p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
            if !hidden {
                collect_md(&p, out)?;
            }
        } else if p.extension().map(|x| x == "md").unwrap_or(false) {
            out.push(p);
        }
    }
    Ok(())
}

/// Infer a record from a plain markdown file (no frontmatter): title from the first `# H1`
/// (else the filename), namespace from the folder path under `root`, kind defaults to memory.
fn infer_record(root: &std::path::Path, file: &std::path::Path, content: &str) -> (entry::Kind, String, String, String) {
    let title = content
        .lines()
        .find_map(|l| l.strip_prefix("# ").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_string());
    let folders = file
        .strip_prefix(root)
        .ok()
        .and_then(|p| p.parent())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let folders = folders
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase().replace(' ', "-"))
        .collect::<Vec<_>>()
        .join("/");
    let ns = if folders.is_empty() { "resources/notes".to_string() } else { format!("resources/{folders}") };
    (entry::Kind::Memory, ns, title, content.to_string())
}

fn wired(config_path: &std::path::Path, needle: &str) -> &'static str {
    match std::fs::read_to_string(config_path) {
        Ok(s) if s.contains(needle) => "wired",
        Ok(_) => "present, not wired",
        Err(_) => "not found",
    }
}

fn status() -> Result<()> {
    println!("dmem {} - daimon-memory v2", env!("CARGO_PKG_VERSION"));
    #[cfg(feature = "client")]
    if let Some(link) = config::server_link() {
        let m = Memory::open()?;
        println!("mode   : remote client");
        println!("server : {}", link.url);
        println!("recall : {}", m.recall_mode());
        return Ok(());
    }
    let tenant = config::tenant();
    let db = config::db_path(&tenant)?;
    println!("mode   : embedded (local fallback, deprecated; run `dmem setup` for client/server)");
    println!("tenant : {}", tenant);
    println!("store  : {}", db.display());
    let m = Memory::open()?;
    let counts = m.counts()?;
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    println!("records: {} live", total);
    println!("recall : {}", m.recall_mode());
    for (k, n) in counts {
        println!("  {:<18} {}", k, n);
    }
    if let Some(h) = dirs::home_dir() {
        println!("devin  : {}", wired(&h.join(".config/devin/config.json"), "dmem hook"));
        println!("claude : {}", wired(&h.join(".claude/settings.json"), "dmem hook"));
    }
    Ok(())
}
