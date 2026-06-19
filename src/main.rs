//! dm - daimon-memory v2 (embedded mode). A small typed memory engine with hybrid
//! recall, behind a MemoryStore trait. M0: SQLite + FTS keyword recall + Devin/Claude
//! hooks. LanceDB + dense vectors layer in next behind the same trait.

mod bootstrap;
mod config;
mod entry;
mod hooks;
mod mcp;
mod render;
mod sqlite;
mod store;
mod tools;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tools::Memory;

#[derive(Parser)]
#[command(name = "dm", version, about = "daimon-memory v2: small embedded typed memory with hybrid recall")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum Cmd {
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
    },
    /// Show recent high-importance memory.
    Recent {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Run as an MCP stdio server (recall + typed save tools for MCP-aware agents).
    Mcp,
}

#[derive(Subcommand)]
#[command(rename_all = "snake_case")]
enum HookCmd {
    /// SessionStart: inject persona/protocol + recent context.
    SessionStart,
    /// UserPromptSubmit: recall for the prompt (read from stdin JSON or arg).
    UserPromptSubmit { prompt: Vec<String> },
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
        Cmd::Bootstrap { devin, claude, all } => {
            bootstrap::run(devin || all, claude || all)
        }
        Cmd::Hook(HookCmd::SessionStart) => hooks::session_start(),
        Cmd::Hook(HookCmd::UserPromptSubmit { prompt }) => {
            let arg = if prompt.is_empty() { None } else { Some(prompt.join(" ")) };
            hooks::user_prompt_submit(arg)
        }
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
        Cmd::Recall { query, limit } => {
            let q = query.join(" ");
            let hits = Memory::open()?.recall(&q, limit)?;
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
        Cmd::Mcp => mcp::serve(),
    }
}
