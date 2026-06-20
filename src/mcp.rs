//! Minimal MCP stdio server: newline-delimited JSON-RPC 2.0 over stdin/stdout, exposing
//! dm's recall + typed save tools so an MCP-aware agent (Devin, etc.) can read and write
//! memory in-session. Hand-rolled (no SDK dep) for a small, reliable surface.

use crate::render;
use crate::tools::Memory;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn tool_schemas() -> Value {
    json!([
        {
            "name": "recall",
            "description": "Recall relevant shared memory (typed, deterministic). Returns matching records.",
            "inputSchema": {"type":"object","properties":{
                "query":{"type":"string","description":"what to recall"},
                "limit":{"type":"integer","description":"max results (default 6)"}
            },"required":["query"]}
        },
        {
            "name": "remember",
            "description": "Store a memory record. Free-form by default; pass `kind` (and ideally `title`) to store a TYPED record: runbook, project_convention, service_topology, known_failure_mode, remediation_pattern, resource_summary, persona, protocol, reminder, memory.",
            "inputSchema": {"type":"object","properties":{
                "text":{"type":"string","description":"the content / body of the record"},
                "kind":{"type":"string","description":"optional typed kind (omit for a plain memory)"},
                "title":{"type":"string","description":"title for a typed record (defaults to the first line of `text`)"},
                "namespace":{"type":"string","description":"e.g. resources/notes or agent/lessons"}
            },"required":["text"]}
        },
        {
            "name": "log_decision",
            "description": "Store a typed Decision (records a non-obvious choice and why).",
            "inputSchema": {"type":"object","properties":{
                "title":{"type":"string"},
                "context":{"type":"string"},
                "decision":{"type":"string"},
                "rationale":{"type":"string"},
                "namespace":{"type":"string"}
            },"required":["title","decision"]}
        },
        {
            "name": "log_lesson",
            "description": "Store a typed AgentLesson (a reusable insight or a corrected mistake, phrased to prevent repeating it).",
            "inputSchema": {"type":"object","properties":{
                "title":{"type":"string"},
                "lesson":{"type":"string"},
                "namespace":{"type":"string","description":"defaults to agent/lessons"}
            },"required":["title","lesson"]}
        },
        {
            "name": "log_incident",
            "description": "Store a typed IncidentSummary (something failed/broke/reverted: its impact and resolution).",
            "inputSchema": {"type":"object","properties":{
                "title":{"type":"string"},
                "impact":{"type":"string"},
                "resolution":{"type":"string"},
                "namespace":{"type":"string"}
            },"required":["title","impact"]}
        },
        {
            "name": "add_reminder",
            "description": "Store a typed Reminder (a dated or pending follow-up).",
            "inputSchema": {"type":"object","properties":{
                "title":{"type":"string"},
                "text":{"type":"string"},
                "namespace":{"type":"string"}
            },"required":["title","text"]}
        },
        {
            "name": "forget",
            "description": "Retract a record by its daimon:// uri (drops it from recall, keeps history).",
            "inputSchema": {"type":"object","properties":{
                "uri":{"type":"string"}
            },"required":["uri"]}
        }
    ])
}

fn s<'a>(args: &'a Value, k: &str) -> &'a str {
    args.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// The serverInfo reported in the MCP `initialize` response. Named "dmem" to match the
/// binary (the old short name "dm" was a leftover from the dm -> dmem rename).
fn server_info() -> Value {
    json!({"name": "dmem", "version": env!("CARGO_PKG_VERSION")})
}

/// Run a tool; return the text content (or an error string).
fn call_tool(mem: &Memory, name: &str, args: &Value) -> std::result::Result<String, String> {
    match name {
        "recall" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            let hits = mem.recall(s(args, "query"), limit).map_err(|e| e.to_string())?;
            let block = render::render_recall(&hits);
            Ok(if block.is_empty() { "(no matches)".into() } else { block })
        }
        "remember" => {
            let ns = if s(args, "namespace").is_empty() { "resources/notes" } else { s(args, "namespace") };
            let text = s(args, "text");
            let kind_str = s(args, "kind");
            let uri = if kind_str.is_empty() {
                mem.remember(text, ns).map_err(|e| e.to_string())?
            } else {
                let kind = crate::entry::Kind::from_str(kind_str)
                    .ok_or_else(|| format!("unknown kind: {kind_str}"))?;
                let title = if s(args, "title").is_empty() { crate::tools::first_line(text) } else { s(args, "title").to_string() };
                mem.import_record(kind, ns, &title, text).map_err(|e| e.to_string())?
            };
            Ok(format!("stored {}", uri))
        }
        "log_decision" => {
            let ns = if s(args, "namespace").is_empty() { "resources/decisions" } else { s(args, "namespace") };
            let uri = mem
                .log_decision(s(args, "title"), s(args, "context"), s(args, "decision"), s(args, "rationale"), ns)
                .map_err(|e| e.to_string())?;
            Ok(format!("stored {}", uri))
        }
        "log_lesson" => {
            let ns = if s(args, "namespace").is_empty() { "agent/lessons" } else { s(args, "namespace") };
            let uri = mem.log_lesson(s(args, "title"), s(args, "lesson"), ns).map_err(|e| e.to_string())?;
            Ok(format!("stored {}", uri))
        }
        "log_incident" => {
            let ns = if s(args, "namespace").is_empty() { "resources/incidents" } else { s(args, "namespace") };
            let uri = mem
                .log_incident(s(args, "title"), s(args, "impact"), s(args, "resolution"), ns)
                .map_err(|e| e.to_string())?;
            Ok(format!("stored {}", uri))
        }
        "add_reminder" => {
            let ns = if s(args, "namespace").is_empty() { "agent/reminders" } else { s(args, "namespace") };
            let uri = mem.add_reminder(s(args, "title"), s(args, "text"), ns).map_err(|e| e.to_string())?;
            Ok(format!("stored {}", uri))
        }
        "forget" => {
            let n = mem.forget(s(args, "uri")).map_err(|e| e.to_string())?;
            Ok(if n == 0 { "nothing to forget".into() } else { format!("forgot {} ({} retired)", s(args, "uri"), n) })
        }
        other => Err(format!("unknown tool: {}", other)),
    }
}

pub fn serve() -> Result<()> {
    let mem = Memory::open()?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // Notifications (no id) get no response.
        let result: Option<Value> = match method {
            "initialize" => {
                let pv = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05")
                    .to_string();
                Some(json!({
                    "protocolVersion": pv,
                    "capabilities": {"tools": {}},
                    "serverInfo": server_info()
                }))
            }
            "tools/list" => Some(json!({"tools": tool_schemas()})),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_err) = match call_tool(&mem, name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (format!("error: {}", e), true),
                };
                Some(json!({"content": [{"type": "text", "text": text}], "isError": is_err}))
            }
            "ping" => Some(json!({})),
            _ => None, // notifications/initialized, etc.
        };

        if let (Some(id), Some(result)) = (id, result) {
            let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(out, "{}", msg)?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_reports_dmem() {
        assert_eq!(server_info()["name"], "dmem");
        assert_eq!(server_info()["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn exposes_full_typed_save_surface() {
        let tools = tool_schemas();
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        for t in ["recall", "remember", "log_decision", "log_lesson", "log_incident", "add_reminder", "forget"] {
            assert!(names.contains(&t), "MCP surface missing tool: {t}");
        }
        // remember is kind-aware so the governance's "remember kind=runbook/project_convention/..." works
        let remember = tools.as_array().unwrap().iter().find(|t| t["name"] == "remember").unwrap();
        assert!(remember["inputSchema"]["properties"].get("kind").is_some(), "remember must accept a kind");
    }
}
