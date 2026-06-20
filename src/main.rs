//! dm - daimon-memory v2 (embedded mode). A small typed memory engine with hybrid
//! recall, behind a MemoryStore trait. M0: SQLite + FTS keyword recall + Devin/Claude
//! hooks. LanceDB + dense vectors layer in next behind the same trait.

mod bootstrap;
#[cfg(feature = "client")]
mod client;
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
mod server;
#[cfg(feature = "wizard")]
mod setup;
#[cfg(feature = "self-update")]
mod upgrade;
#[cfg(feature = "zvec")]
mod zvec_index;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tools::Memory;

#[derive(Parser)]
#[command(name = "dmem", version, about = "daimon-memory v2: small embedded typed memory with hybrid recall")]
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
    /// Detect agents and install dm's lifecycle hooks (Devin, Claude Code).
    Bootstrap {
        #[arg(long)]
        devin: bool,
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        all: bool,
    },
    /// Lifecycle hook handlers (called by the agent; emit context on stdout).
    #[command(subcommand)]
    Hook(HookCmd),
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
    },
    /// Recall memory for a query (human-readable).
    Recall {
        query: Vec<String>,
        #[arg(long, default_value_t = 6)]
        limit: usize,
        /// Bitemporal: recall the store AS OF this epoch-ms (and facts valid then).
        #[arg(long = "as-of", visible_alias = "as_of")]
        as_of: Option<i64>,
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
    /// Show store + wiring status.
    Status,
    /// Run as an MCP stdio server (recall + typed save tools for MCP-aware agents).
    Mcp,
    /// Run the network API server (multi-token bearer -> tenant). Needs --features server.
    #[cfg(feature = "server")]
    Serve {
        #[arg(long, default_value = "127.0.0.1:8077")]
        addr: String,
    },
    /// Update dmem in place from GitHub Releases. Needs --features self-update.
    #[cfg(feature = "self-update")]
    Upgrade {
        /// include pre-releases (rc/beta), not just stable
        #[arg(long)]
        pre: bool,
    },
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
        Cmd::Bootstrap { devin, claude, all } => {
            bootstrap::run(devin || all, claude || all)
        }
        Cmd::Hook(HookCmd::SessionStart) => hooks::session_start(),
        Cmd::Hook(HookCmd::UserPromptSubmit { prompt }) => {
            let arg = if prompt.is_empty() { None } else { Some(prompt.join(" ")) };
            hooks::user_prompt_submit(arg)
        }
        Cmd::Hook(HookCmd::SessionEnd) => hooks::session_end(),
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
        Cmd::Remember { text, namespace } => {
            let uri = Memory::open()?.remember(&text, &namespace)?;
            println!("stored {}", uri);
            Ok(())
        }
        Cmd::Recall { query, limit, as_of } => {
            let q = query.join(" ");
            let m = Memory::open()?;
            let hits = match as_of {
                Some(ts) => m.recall_as_of(&q, limit, ts, ts)?,
                None => m.recall(&q, limit)?,
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
        Cmd::Status => status(),
        Cmd::Mcp => mcp::serve(),
        #[cfg(feature = "server")]
        Cmd::Serve { addr } => server::run_blocking(&addr),
        #[cfg(feature = "self-update")]
        Cmd::Upgrade { pre } => upgrade::run(pre),
    }
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
    println!("mode   : embedded");
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
