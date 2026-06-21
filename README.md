# dm-lite

daimon-memory v2: a small typed memory engine for AI agents. One binary, client/server: run `dmem serve` locally (a managed loopback daemon) or on a host, and point the CLI and agent hooks at it (local versus remote is just the URL). Hybrid recall (keyword and dense vector), bitemporal history, and multitenant storage (one database per tenant).

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

It detects your agents (Claude, Devin, Codex, Hermes), wires the ones you pick (nothing is pre-selected), and seeds a first memory.

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

One command per agent. It installs the lifecycle hooks (persona on session start, relevant memory recalled each turn, a save nudge) and registers the dmem MCP server (the remember / recall / log_* tools), then migrates off any older daimon wiring.

```bash
dmem bootstrap --claude    # Claude Code
dmem bootstrap --devin     # Devin CLI
dmem bootstrap --codex     # Codex
dmem bootstrap --hermes    # Hermes
dmem bootstrap --all       # every agent detected
dmem bootstrap --remove --claude   # undo (per agent, or --all)
```

Each harness injects differently:

- Claude, Devin: hooks live in the agent config (settings.json, config.json); the MCP server is added through `claude mcp add` / `devin mcp add`. Injection is silent.
- Codex: MCP lives in `~/.codex/config.toml`; the hooks ship as a small plugin that bootstrap installs with `codex plugin add` (the `codex` CLI must be on PATH, else run `codex plugin add dmem@dmem` yourself). On the next session Codex asks once to TRUST the hooks, and echoes the injected context to the terminal (expected).
- Hermes: MCP lives in `~/.hermes/config.yaml`; recall rides a `pre_llm_call` hook; persona and protocols are projected into `~/.hermes/SOUL.md` (Hermes's always-on identity), a snapshot you refresh by re-running `dmem bootstrap --hermes` after changing them. Restart Hermes once after wiring.

For any other MCP client, dmem serves it over stdio:

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

Each tenant gets its own database file. Routes mirror the local tools: `POST /recall /recent /history /forget /remember /log_decision /log_lesson /log_incident /log_runbook /log_convention /add_reminder /import /persona`, plus admin routes and an open `GET /healthz`.

## Run as a managed service

Run dmem as a login service (launchd on macOS, systemd `--user` on Linux) instead of `dmem serve` in a terminal. It starts on login; stopping it reclaims the model's RAM.

```bash
dmem service install     # write the unit + [server] block, then start it
dmem service status
dmem service stop
dmem service start
dmem service restart
dmem service uninstall
```

`install` also points this machine's config at the daemon, so the CLI and agent hooks become thin clients of the running process.

## Multitenant admin (token-only, no passwords)

On first start the server generates a **root admin token** (no tenant, no memory), prints it once, and writes it to `<data>/admin.token` (0600). The admin creates tenants and issues one-time tokens; each token isolates one tenant's memory.

```bash
# admin wires the admin token once, then manages tenants
dmem login https://server:8077 <admin-token>
dmem admin add acme --label laptop     # creates the silo + prints a one-time member token
dmem admin list
dmem admin revoke <token|tenant>
dmem admin rm <tenant>
```

The admin hands the member token to the user. There are no passwords: the token is the credential and the isolation key. A lost token is revoked and reissued. `DM_TOKEN_<tenant>` env vars still work as a quick static fallback.

## Connect to a server (remote client)

The user installs their token and is done:

```bash
dmem login https://server:8077 <token>        # --insecure or --ca-cert <pem> for self-signed
dmem recall "..."                              # now served by the remote, in the user's tenant
dmem logout                                    # disconnect
```

`dmem login` writes `~/.config/dmem/config.toml` (`[server]` block, 0600). From then on the CLI and the agent hooks all talk to the server over TLS with that token instead of local memory. `dmem status` shows the connection. `dmem setup` can do this interactively.

## Persona and governance

The agent's identity and capture rules live in memory (injected each session), not in dotfiles.

```bash
dmem template export ~/dmem-templates   # persona skeleton + two generic governance protocols
# edit persona.md (fill the <PLACEHOLDERS>)
dmem import ~/dmem-templates/           # load them as records
```

`dmem setup` also asks your AI's name and your name and sets a default persona plus the generic governance for you. Nothing personal ships in the binary; the templates are blank skeletons.

**dm-lite is the single source of governance.** Your agents' persona and protocols live here and are projected into each agent when you wire it. Static agent-instruction files like `AGENTS.md` (or a stray global `CLAUDE.md`/`SOUL.md` copy) compete with that and become noise: an agent that also reads an `AGENTS.md` picks up a second, drifting identity, and any topology or facts baked into it go stale the moment memory moves on. Some tools auto-create one (e.g. Codex desktop writes `~/AGENTS.md` on import from Claude Code). If you wire dm-lite, remove or empty any global `AGENTS.md` you did not deliberately author; keep only the repo-scoped ones you actually maintain.

## Migrate in

```bash
# from daimon-memory v1 (JSONL export), preserving original timestamps:
dmem migrate --url https://v1-host:8080 --token <admin-token>
dmem migrate --file export.jsonl          # or - for stdin

# from a folder of markdown (e.g. an Obsidian vault):
dmem import ~/vault/                       # folder -> namespace, # H1 / filename -> title
```

`migrate` carries each v1 record across 1:1 (dm-lite's kinds match v1 exactly) and preserves its original creation time (as valid-time) and importance. `import` walks the tree, uses frontmatter when present and infers otherwise.

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
