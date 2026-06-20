# dm-lite

daimon-memory v2: a small typed memory engine for AI agents. One binary, three modes (embedded, server, or a client connected to a server), with hybrid recall (keyword and dense vector), bitemporal history, and multitenant storage (one database per tenant).

## Install

Grab the archive for your OS from the [latest release](https://github.com/wakbijok/dm-lite/releases). Each one holds `dmem` plus its native vector library; keep them in the same folder.

```bash
tar xzf dmem-*-x86_64-unknown-linux-gnu.tar.gz
install -m755 dmem ~/.local/bin/dmem
cp libzvec_c_api.* ~/.local/bin/      # the lib must sit next to the binary
dmem status
```

On Windows: unzip, drop `dmem.exe` and `zvec_c_api.dll` in the same folder, add it to PATH.

## First run

The download is not code signed, so the OS flags it once:

- macOS: clear the quarantine flag, `xattr -dr com.apple.quarantine ~/.local/bin/dmem ~/.local/bin/libzvec_c_api.dylib`
- Windows: SmartScreen may warn; pick "More info", then "Run anyway"

Then let the wizard set you up:

```bash
dmem setup
```

It detects your agents (Devin, Claude), wires the hooks you pick, and seeds a first memory.

## Save and recall

```bash
dmem remember "Devin is the Windsurf lineage"
dmem log_decision --title "Bet on zvec" --decision "use zvec as the vector store" --rationale "in-process, small, fast"
dmem log_lesson   --title "AVX2 gate"   --lesson "the embedder needs AVX2"
dmem add_reminder --title "ship rc"     --text "tag the release candidate"

dmem recall "vector store decision"
dmem recent
dmem forget "daimon://resources/notes/memory/..."   # retract a record (keeps its history)
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

The same binary serves many tenants over HTTP(S), one bearer token per tenant. TLS is built in (no reverse proxy assumed).

```bash
export DM_TOKEN_ACME=<secret>          # this token maps to tenant "acme"

dmem serve --addr 0.0.0.0:8077 --tls-generate                 # self-signed cert (saved under the data dir)
dmem serve --addr 0.0.0.0:8077 --tls-cert cert.pem --tls-key key.pem   # your own cert
dmem serve --addr 127.0.0.1:8077                              # plain HTTP (local only; warns)
```

```bash
curl -sk https://localhost:8077/healthz
curl -sk -X POST https://localhost:8077/recall \
  -H 'authorization: Bearer <secret>' \
  -H 'content-type: application/json' \
  -d '{"query":"vector store","limit":5}'
```

Routes: `POST /recall`, `/recent`, `/history`, `/forget`, `/remember`, `/log_decision`, `/log_lesson`, `/log_incident`, `/log_runbook`, `/log_convention`, `/add_reminder`, `/persona`, and an open `GET /healthz`. Each tenant gets its own database file.

## Connect to a server (remote client)

Point this dmem (CLI and the agent hooks) at a remote server. Run `dmem setup` and choose "connect to a memory server", or write `~/.config/dmem/config.toml`:

```toml
[server]
url   = "https://memory.myhost.tld:8077"
token = "<secret>"
# insecure = true          # accept a self-signed cert, OR:
# ca_cert  = "/path/cert.pem"   # trust a specific cert/CA
```

With that in place, `dmem recall` / `dmem remember` and the hooks all talk to the server over TLS with your token, instead of local memory. `dmem status` shows the connection.

## Keep it updated

```bash
dmem upgrade           # latest stable release
dmem upgrade --pre     # include release candidates (rc/beta)
```

It replaces the binary in place. Your data lives in a separate directory and is never touched; the schema migrates forward on the next run.

## Build from source

```bash
cargo build --release                             # pure Rust, keyword recall, no models
cargo build --release --features zvec,fastembed   # add dense vector plus bge-small semantics
cargo build --release --features dist             # the full release binary (all of the below)
```

The default build is offline and light. `zvec` adds the dense vector store (downloads a prebuilt native lib). `fastembed` adds real bge-small embeddings (downloads the model once). `server` adds the axum HTTP API with built-in TLS. `client` adds remote-client mode. `wizard` adds `dmem setup`. `self-update` adds `dmem upgrade`. `dist` bundles all of them, which is what the release binaries ship.

License: MIT.
