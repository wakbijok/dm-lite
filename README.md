# dm-lite

daimon-memory v2: a small typed memory engine for AI agents. One binary, two modes (embedded client and network server), with hybrid recall (keyword and dense vector), bitemporal history, and multitenant storage (one database per tenant).

## Install

Grab the archive for your OS from the [latest release](https://github.com/wakbijok/dm-lite/releases). Each one holds `dmem` plus its native vector library; keep them in the same folder.

```bash
tar xzf dmem-*-x86_64-unknown-linux-gnu.tar.gz
install -m755 dmem ~/.local/bin/dmem
cp libzvec_c_api.* ~/.local/bin/      # the lib must sit next to the binary
dmem status
```

On Windows: unzip, drop `dmem.exe` and `zvec_c_api.dll` in the same folder, add it to PATH.

## Save and recall

```bash
dmem remember "Devin is the Windsurf lineage"
dmem log_decision --title "Bet on zvec" --decision "use zvec as the vector store" --rationale "in-process, small, fast"
dmem log_lesson   --title "AVX2 gate"   --lesson "the embedder needs AVX2"
dmem add_reminder --title "ship rc"     --text "tag the release candidate"

dmem recall "vector store decision"
dmem recent
```

Recall fuses keyword (SQLite FTS5) and dense vector (zvec with bge-small embeddings), ranked together. The release binaries ship both; a plain source build does keyword recall only.

## Time travel

Every save is a new version; nothing is overwritten.

```bash
dmem history "daimon://resources/notes/decision/bet-on-zvec"      # full lineage
dmem recall "vector store" --as-of 1718000000000                  # the store as of an epoch in ms
```

## Wire it into your agent

This installs lifecycle hooks, so the agent gets relevant memory injected each turn and a nudge to capture decisions at session end.

```bash
dmem bootstrap --devin     # Devin CLI
dmem bootstrap --claude    # Claude Code
```

dmem also speaks MCP over stdio:

```bash
dmem mcp
```

## Run as a server

The same binary serves many tenants over HTTP, one bearer token per tenant.

```bash
export DM_TOKEN_ACME=<secret>          # this token maps to tenant "acme"
dmem serve --addr 0.0.0.0:8077
```

```bash
curl -s localhost:8077/healthz
curl -s -X POST localhost:8077/recall \
  -H 'authorization: Bearer <secret>' \
  -H 'content-type: application/json' \
  -d '{"query":"vector store","limit":5}'
```

Routes: `POST /recall`, `/remember`, `/log_decision`, `/add_reminder`, and an open `GET /healthz`. Each tenant gets its own database file.

## Build from source

```bash
cargo build --release                             # pure Rust, keyword recall, no models
cargo build --release --features zvec,fastembed   # add dense vector plus bge-small semantics
cargo build --release --features server           # add the HTTP server
```

The default build is offline and light. `zvec` adds the dense vector store (downloads a prebuilt native lib). `fastembed` adds real bge-small embeddings (downloads the model once). `server` adds the axum HTTP API.

License: MIT.
