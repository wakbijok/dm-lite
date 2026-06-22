//! Minimal MCP stdio server: newline-delimited JSON-RPC 2.0 over stdin/stdout. Hand-rolled (no
//! SDK dep) for a small, reliable surface. This is how dmem reaches MCP clients that have NO
//! lifecycle hooks (Claude Desktop and most MCP hosts): besides the recall + typed save TOOLS,
//! `initialize` carries the persona + protocols in the `instructions` field, and a `bootstrap`
//! prompt (persona + protocols + open reminders) plus a `recall` prompt give on-demand governance.
//! Hook-wired hosts (Claude Code, Devin, Codex) still get persona/recall through their hooks. The
//! server is mode-agnostic: it reads/writes through the Memory enum, so it works the same in the
//! daemon/client path (the default) and the deprecated embedded fallback.

use crate::render;
use crate::tools::Memory;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn tool_schemas() -> Value {
    json!([
        {
            "name": "recall",
            "description": "Recall relevant shared memory (typed; ranked by relevance and runtime signals, hybrid FTS + vector). Returns matching records.",
            "inputSchema": {"type":"object","properties":{
                "query":{"type":"string","description":"what to recall"},
                "limit":{"type":"integer","description":"max results (default 6, capped at 1000)"}
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

/// The prompts this server advertises (mirrors `tool_schemas`). The LIST is static; the rendered
/// CONTENT (fetched per `prompts/get`) is live. `bootstrap` is the must-have for hook-less clients;
/// `recall` makes on-demand recall a one-click prompt, narrowing the no-per-turn-recall gap.
fn prompt_schemas() -> Value {
    json!([
        {
            "name": "bootstrap",
            "title": "Load dmem persona + protocols + open reminders",
            "description": "Inject the shared persona, the operating protocols (how to work, and when/what to persist), and the current open reminders. Fire this at the start of a session so the assistant adopts the same governance the hook-wired tools get automatically."
        },
        {
            "name": "recall",
            "title": "Recall dmem memory",
            "description": "Recall relevant shared memory for a query, ranked by relevance and runtime signals (hybrid FTS + vector).",
            "arguments": [
                {"name": "query", "description": "what to recall", "required": true}
            ]
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
            // Clamp untrusted limit: bounds the deeper rescoring pool (limit*2) and the SQL
            // `LIMIT ?` cast, so a hostile value can't wrap or balloon the query.
            let limit = (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(6) as usize).min(1000);
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

// JSON-RPC 2.0 error codes we emit (a subset of the standard set).
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Protocol revisions we implement, newest first. Both define tools + prompts + the
/// `instructions` field identically, so we can honor whichever a client speaks.
const SUPPORTED_PROTOCOLS: [&str; 2] = ["2025-03-26", "2024-11-05"];

/// Build the `initialize` result. Negotiates the protocol version (echo the client's if we
/// support it, else answer with our latest) and includes `instructions` only when present.
fn initialize_result(instructions: &Option<String>, params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let pv = match requested {
        Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v,
        _ => SUPPORTED_PROTOCOLS[0],
    };
    let mut result = json!({
        "protocolVersion": pv,
        "capabilities": {"tools": {}, "prompts": {"listChanged": false}},
        "serverInfo": server_info(),
    });
    if let Some(instr) = instructions {
        if !instr.is_empty() {
            result["instructions"] = json!(instr);
        }
    }
    result
}

/// `tools/call`: validate at the protocol level (missing/empty name -> invalid params), then run
/// the tool. A tool that FAILS is reported in-band via `isError` (the MCP tool convention),
/// distinct from a JSON-RPC protocol error.
fn tools_call(mem: &Memory, params: &Value) -> std::result::Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err((INVALID_PARAMS, "tools/call requires a non-empty 'name'".into()));
    }
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let (text, is_err) = match call_tool(mem, name, &args) {
        Ok(t) => (t, false),
        Err(e) => (format!("error: {}", e), true),
    };
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_err}))
}

/// `prompts/get`: assemble one user message for the named prompt. Returns a JSON-RPC error for an
/// unknown prompt or a missing required argument (NOT a tool-style isError, since these are
/// protocol-level problems). `bootstrap` is governance + open items; `recall` is on-demand recall.
fn get_prompt(mem: &Memory, params: &Value) -> std::result::Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let (description, text) = match name {
        "bootstrap" => {
            let persona = mem.persona().unwrap_or_default();
            let reminders = mem.reminders(8).unwrap_or_default();
            let body = render::render_bootstrap(&persona, &reminders);
            let text = if body.is_empty() {
                "(no persona, protocols, or open reminders are seeded yet)".to_string()
            } else {
                body
            };
            ("dmem persona, protocols, and current open reminders".to_string(), text)
        }
        "recall" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
            if query.is_empty() {
                return Err((INVALID_PARAMS, "the 'recall' prompt requires a non-empty 'query' argument".into()));
            }
            let hits = mem.recall(query, 8).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
            let block = render::render_recall(&hits);
            let text = if block.trim().is_empty() { "(no matches)".to_string() } else { block };
            (format!("dmem recall for: {query}"), text)
        }
        other => return Err((INVALID_PARAMS, format!("unknown prompt: {other}"))),
    };
    Ok(json!({
        "description": description,
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}],
    }))
}

/// Build the JSON-RPC response for one request. `None` means "no response" (a notification: any
/// request without an `id`). An `id`-bearing request ALWAYS gets a reply (result or error), so a
/// client never blocks waiting on a dropped request.
fn handle(mem: &Memory, instructions: &Option<String>, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?; // no id -> notification -> stay silent
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let outcome: std::result::Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result(instructions, &params)),
        "tools/list" => Ok(json!({"tools": tool_schemas()})),
        "tools/call" => tools_call(mem, &params),
        "prompts/list" => Ok(json!({"prompts": prompt_schemas()})),
        "prompts/get" => get_prompt(mem, &params),
        "ping" => Ok(json!({})),
        other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

pub fn serve() -> Result<()> {
    let mem = Memory::open()?;
    // Persona/protocols are stable for the process lifetime, so render the instructions field
    // once rather than paying a (possibly remote) persona round-trip on every initialize. If
    // persona is empty or the backing store is unreachable at startup, the field is omitted and
    // the server still serves its tools + prompts.
    let instructions: Option<String> = mem
        .persona()
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| render::render_instructions(&p))
        .filter(|s| !s.is_empty());
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
            Err(_) => {
                // Unparseable line: reply with a parse error (null id, per JSON-RPC) instead of
                // going silent, so a client never blocks on a dropped request.
                let msg = json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": PARSE_ERROR, "message": "parse error"}});
                writeln!(out, "{}", msg)?;
                out.flush()?;
                continue;
            }
        };
        if let Some(resp) = handle(&mem, &instructions, &req) {
            writeln!(out, "{}", resp)?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;
    use crate::tools::LocalMemory;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A cheap local Memory over a private temp store (no env, no model load via for_test).
    fn test_mem() -> Memory {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dmmcp-{}-{}-{}", std::process::id(), crate::entry::now_ms(), n));
        std::fs::create_dir_all(&dir).unwrap();
        Memory::Local(LocalMemory::for_test(SqliteStore::open(&dir.join("t.db")).unwrap()))
    }

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
        // every kind the remember description names must actually parse (governance kinds matter).
        for k in ["runbook", "project_convention", "service_topology", "known_failure_mode",
                  "remediation_pattern", "resource_summary", "persona", "protocol", "reminder", "memory"] {
            assert!(crate::entry::Kind::from_str(k).is_some(), "advertised kind does not parse: {k}");
        }
    }

    #[test]
    fn recall_tool_description_is_not_claimed_deterministic() {
        let tools = tool_schemas();
        let recall = tools.as_array().unwrap().iter().find(|t| t["name"] == "recall").unwrap();
        assert!(!recall["description"].as_str().unwrap().contains("deterministic"));
    }

    #[test]
    fn initialize_advertises_prompts_and_tools_and_instructions() {
        let r = initialize_result(&Some("RULES".to_string()), &json!({"protocolVersion": "2024-11-05"}));
        assert_eq!(r["capabilities"]["prompts"]["listChanged"], false);
        assert!(r["capabilities"]["tools"].is_object(), "tools capability must remain");
        assert_eq!(r["instructions"], "RULES");
        assert_eq!(r["protocolVersion"], "2024-11-05", "supported version is echoed");
    }

    #[test]
    fn initialize_negotiates_unknown_protocol_and_omits_empty_instructions() {
        let r = initialize_result(&None, &json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r["protocolVersion"], "2025-03-26", "unknown version negotiated to our latest");
        assert!(r.get("instructions").is_none(), "no instructions field when persona is absent");
    }

    #[test]
    fn prompt_schemas_exposes_bootstrap_and_recall() {
        let ps = prompt_schemas();
        let names: Vec<&str> = ps.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"bootstrap") && names.contains(&"recall"));
        let recall = ps.as_array().unwrap().iter().find(|p| p["name"] == "recall").unwrap();
        assert_eq!(recall["arguments"][0]["name"], "query");
        assert_eq!(recall["arguments"][0]["required"], true);
    }

    #[test]
    fn handle_replies_to_id_bearing_unknown_method_and_stays_silent_on_notifications() {
        let mem = test_mem();
        let unknown = handle(&mem, &None, &json!({"jsonrpc": "2.0", "id": 1, "method": "bogus/method"})).unwrap();
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
        // a notification (no id) never gets a reply, even for an unknown method
        assert!(handle(&mem, &None, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        // ping is answered with an empty result
        let ping = handle(&mem, &None, &json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})).unwrap();
        assert!(ping.get("result").is_some());
    }

    #[test]
    fn prompts_get_and_tools_call_validate_params() {
        let mem = test_mem();
        // unknown prompt -> invalid params
        let unknown = handle(&mem, &None, &json!({"id": 1, "method": "prompts/get", "params": {"name": "nope"}})).unwrap();
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        // recall prompt with no query -> invalid params (does not touch the store)
        let no_q = handle(&mem, &None, &json!({"id": 2, "method": "prompts/get", "params": {"name": "recall", "arguments": {}}})).unwrap();
        assert_eq!(no_q["error"]["code"], INVALID_PARAMS);
        // tools/call with no name -> invalid params (not a tool isError)
        let no_name = handle(&mem, &None, &json!({"id": 3, "method": "tools/call", "params": {}})).unwrap();
        assert_eq!(no_name["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn prompts_get_bootstrap_returns_one_user_message() {
        let mem = test_mem();
        let resp = handle(&mem, &None, &json!({"id": 1, "method": "prompts/get", "params": {"name": "bootstrap"}})).unwrap();
        let msgs = resp["result"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"]["type"], "text");
        assert!(msgs[0]["content"]["text"].as_str().is_some());
    }
}
