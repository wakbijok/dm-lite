<p align="center">
  <img src="assets/dm-lite-banner.png" alt="dm-lite, the daimon-memory successor" width="840">
</p>

# dm-lite

Memory for Hermes, Codex, Claude Code, Claude Cowork, OpenCode, Devin, Grok CLI, Claude Desktop, and any MCP agent. dm-lite is daimon-memory v2: one small typed memory engine in a single binary. Run `dmem serve` locally (a managed loopback daemon) or on a host, then point the CLI and agent hooks at it; local versus remote is just the URL. Hybrid recall (keyword and dense vector), bitemporal history, and multitenant storage (one database per tenant).

## Features

- One memory across your AI tools: the same recall and capture in Hermes, Codex, Claude Code, Claude Cowork, Devin, OpenCode, Grok CLI, and Claude Desktop, with the integration built in (one command per tool), not left for you to wire yourself. Switch tools, keep the same brain. Any MCP client can connect too, and receives the persona and protocols, not only the tools.
- Typed, curated memory: decisions, lessons, incidents, runbooks, conventions, reminders, and more, each a first-class kind.
- Hybrid recall: SQLite FTS5 keyword search fused with dense vectors (candle + bge-small by default, via zvec), ranked together.
- A graph over your memory: link records and mint domain entities (org, product, person, and more), so a recall pulls in a hit's neighbours; browse it offline with `dmem ui`.
- Bitemporal: two time axes, when a fact is true in the world and when you recorded it. Backdate a fact, end its validity, and recall both what was true at a past moment and what you knew at one. Nothing is overwritten.
- Client/server in one binary: run it locally, or host it on a VPS, homelab, or cloud box and point your machines at it. Local or remote is just a URL.
- Multitenant: one database per tenant, token-only IAM (root admin plus per-tenant tokens), built-in TLS (no reverse proxy).
- Per-agent identity on a shared memory: a token can carry an agent label, so each agent is served its own persona (namespace `agents/<agent>/`) while house rules, protocols, and the memory itself stay shared across the tenant; writes are attributed with an `author:<agent>` tag.
- Self-updating: `dmem upgrade` pulls the latest release in place.

## Quickstart

Grab the archive for your OS from the [latest release](https://github.com/wakbijok/dm-lite/releases). Each one holds `dmem` plus its native vector library; keep them together.

```bash
install -m755 dmem ~/.local/bin/dmem
cp libzvec_c_api.* ~/.local/bin/       # the lib must sit next to the binary
dmem setup                             # pick your agents, seed a first memory
```

Save and recall:

```bash
dmem remember "Devin is the Windsurf lineage"
dmem log_decision --title "Bet on zvec" --decision "use zvec as the vector store"
dmem recall "vector store decision"
```

Wire it into an agent (one command each, or `--all`):

```bash
dmem bootstrap --claude     # or --codex / --hermes / --devin / --opencode / --claude-desktop / --cowork / --all
```

### Claude Cowork (macOS)

Cowork is Claude Desktop's agentic mode. It runs the Claude Code engine, but with a per-session config directory, so the hooks `--claude` writes to `~/.claude/settings.json` never load there. `--claude-desktop` alone already gives Cowork the memory tools and the persona (both ride the MCP server), but not the automatic per-prompt recall. `--cowork` closes that gap:

```bash
dmem bootstrap --cowork
```

It ensures the `mcpServers.dmem` entry in `claude_desktop_config.json`, then installs a hooks-only plugin straight into every Cowork workspace on the machine (under the app's `local-agent-mode-sessions/<account>/<org>/cowork_plugins/`, registered in that workspace's `installed_plugins.json` and enabled in `cowork_settings.json`). Cowork's local-plugins reader picks it up at the next session start: SessionStart -> persona + recent context, UserPromptSubmit -> recall. Start a new Cowork session after running it. The plugin bundles no MCP server on purpose; the desktop entry already provides one, and a second copy would duplicate the tools.

The same run also writes `~/.daimon/cowork-plugin/dmem-cowork.plugin`, the documented fallback: if no workspace exists yet (Cowork never opened on this machine), upload that file in Claude Desktop -> Settings -> Plugins. The direct install relies on the app's on-disk plugin layout, which is not a published interface; `dmem status` reports the wiring per workspace so a change in a Desktop update shows up as `not wired` rather than failing silently.

The hooks carry the absolute path of the `dmem` binary. If the binary moves, re-run `dmem bootstrap --cowork`; `dmem status` flags a stale path. To debug a silent hook, set `DM_HOOK_DEBUG=1` in the environment Claude Desktop inherits and read `dmem-hook-debug.log` in the temp dir. `dmem bootstrap --cowork --remove` removes the plugin, registry entry and enabled flag from every workspace and deletes the generated files (the desktop MCP entry stays unless you also pass `--claude-desktop --remove`).

Out of the box this runs on one machine: the server and your client live together. To run the server on one host and connect clients from elsewhere, see the wiki.

## Requirements

Resource use is dominated by the **embedding model the server keeps resident**, not by the SQLite store. Live data is usually tens of megabytes on disk; process RSS is mostly model weights plus the vector index.

**Prebuilt release binaries** ship candle with default model `BAAI/bge-small-en-v1.5`. That is the supported out-of-the-box path. The same binary can load another 384-d Bert checkpoint via env (no recompile). You can also rebuild with a different embedder feature (`model2vec`, `fastembed`, or none for the hash placeholder, which runs only with `DM_REQUIRE_MODEL=0`) if you want a smaller footprint and accept the quality trade-off.

Figures below are **order-of-magnitude, once warm**, for a typical `dmem serve` on a small Linux VPS. OS, concurrent load, and corpus size move the needle; check `dmem doctor` and your process monitor on the real host.

| Setup | Typical process RSS | Languages / notes |
| --- | --- | --- |
| Prebuilt default: candle + `BAAI/bge-small-en-v1.5` | ~0.8-1.2 GiB | English-first. What the release ships. |
| candle + `intfloat/multilingual-e5-small` (env override) | ~1.2-1.5 GiB | Strong pick for **English + Malay (BM/MS)** and other languages. Set mean pooling and e5 role prefixes (see wiki). |
| candle + other 384-d Bert (env override) | similar to model size | Any HF Bert checkpoint with `tokenizer.json` + 384-d hidden size. Match pooling/prefixes to the model card. One that does not embed to a 384-d vector counts as a model that did not load. |
| Build with `model2vec` (e.g. potion-base-8M) | a few hundred MB | Much smaller; weaker semantic recall. |
| No real embedder (hash placeholder) | tens of MB | Keyword/FTS only. Refused unless `DM_REQUIRE_MODEL=0`: tests and development only. |
| Client-only against a remote `dmem serve` | negligible local model RAM | Embeddings run on the server; the client needs no model. |

Disk: model cache is separate from the store (default bge weights ~130 MB on disk; multilingual checkpoints are often larger). After switching models, or after running on the placeholder embedder (`DM_REQUIRE_MODEL=0`, or a 0.3.7 host that logged `using placeholder embedder`), stop the server and run `dmem reindex-embeddings` (once per tenant, `DM_TENANT=<tenant>`) so old and new vectors share one space.

Details, offline cache, and env knobs: wiki [Embedding models](https://github.com/wakbijok/dm-lite/wiki/Embedding-models).

## Per-agent identity (shared memory, separate personas)

Several agents can share one tenant's memory while each receives only its own persona. Mint a token per agent (`dmem admin add <tenant> --agent assistant`, or the env form `DM_TOKEN_<TENANT>__<AGENT>=secret` - double underscore separates tenant from agent). An agent-labelled token cannot read another agent's `agents/<other>/...` tree through ANY surface (recall, recent, history, graph expansion - not just the persona route), cannot write into it, and cannot forget/invalidate records there; its writes are stamped with an `author:<agent>` tag. Everything OUTSIDE the `agents/` tree is one shared pool by design - the shared brain is the point, so do not put per-agent secrets in shared namespaces, and do not share a tenant with agents or users you don't trust (see Security model). Agent-less tokens keep the full legacy behaviour.

## Offline / air-gapped

Hybrid recall uses a small embedding model (prebuilt default: `BAAI/bge-small-en-v1.5`, about 130 MB on disk), downloaded from HuggingFace on first use. dmem requires it: when it cannot load, `dmem serve` does not start and a local tenant (the embedded CLI, `dmem mcp`) does not open. A remote client needs no model. Check readiness before deploying to a sealed network:

```bash
dmem doctor          # active embedder, model, cache dir, whether it is cached, what dmem will do, CPU features
dmem doctor --json   # the same, machine-parseable
```

To run offline, pre-populate the model cache once on a connected machine, then carry it over (or point at a shared path):

```bash
# On a connected machine, fill the cache with the files dmem loads (either line):
HF_HOME=/srv/hf-cache DM_CONFIG=/dev/null DM_DATA_DIR="$(mktemp -d)" dmem recall warmup
HF_HOME=/srv/hf-cache huggingface-cli download BAAI/bge-small-en-v1.5 config.json tokenizer.json model.safetensors
# Carry the model's whole directory over; tar keeps refs/ and the snapshot symlinks:
tar -C /srv/hf-cache/hub -czf bge-small.tgz models--BAAI--bge-small-en-v1.5
# On the sealed host:
mkdir -p /srv/hf-cache/hub && tar -C /srv/hf-cache/hub -xzf bge-small.tgz
HF_HOME=/srv/hf-cache HF_HUB_OFFLINE=1 dmem doctor   # model_cached: yes
HF_HOME=/srv/hf-cache HF_HUB_OFFLINE=1 dmem serve --addr 127.0.0.1:8088
```

The first line needs only the dmem binary: with no config (`DM_CONFIG=/dev/null`, and no `DM_ENDPOINT`) it runs the embedded engine on a throwaway data dir, which loads the model and so downloads it into the cache; a `[server]` block would make it a remote client, which loads no model. The second needs the Python `huggingface_hub` package (`hf download` in newer releases). Python's `snapshot_download('BAAI/bge-small-en-v1.5')` works too, but fetches the whole repository (ONNX and PyTorch weights as well, several times the 130 MB); pass it `allow_patterns=['config.json', 'tokenizer.json', 'model.safetensors']`. dmem reads the standard layout: `hub/models--BAAI--bge-small-en-v1.5/refs/main` names a revision, and `snapshots/<revision>/` holds `config.json`, `tokenizer.json` and `model.safetensors` (symlinks into `blobs/`). Copy the whole directory with `tar`, `cp -a` or `rsync -a` so `refs/` and the symlinks survive; `dmem doctor` counts the model as cached only when that layout is complete.

The model cache is `HUGGINGFACE_HUB_CACHE` if set, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub` (the standard HuggingFace layout, so a cache filled by the Python tools works as is). With `HF_HUB_OFFLINE=1` the model loads from that cache only and nothing is downloaded. `dmem serve` logs the cache dir and model on startup. `dmem doctor` prints the exact directory, whether the model is in it, and what dmem will do (`model_state`), so you know before an upgrade or a first run whether it starts. When the model does not load, the one error names the model, the cache it looked in and how to pre-install the model.

`DM_REQUIRE_MODEL=0` (or `false`, `no`) lets dmem run without the model on a placeholder embedder (keyword overlap, not semantics), for tests and development only: it prints a warning that memory recall is degraded, and `GET /readyz` (its detail) and `dmem doctor` report `degraded: placeholder embedder`. Records saved meanwhile keep placeholder vectors, so run `dmem reindex-embeddings` before using that data dir with the model. Any other value, or none, requires the model; `DM_REQUIRE_MODEL=1` still works and changes nothing.

When a tenant's vector index cannot open, recall falls back to keyword-only for that tenant. For a server that must not degrade silently, turn that into a failure with `DM_REQUIRE_VECTORS=1`: a tenant whose vector index cannot open fails its requests (500) instead of answering keyword-only. The open is retried (see below), so the tenant serves again once the cause is gone. Any value other than empty, `0`, `off`, `false` or `no` turns it on. Without it, `GET /readyz` still reports a tenant without a vector index as not ready.

A tenant's vector index that failed to open is retried in the background, first after 5 s and then at doubling intervals up to 5 min, without waiting for a request. Once it opens, readiness returns and the records written while the tenant had no index are re-embedded (hybrid recall does not return a record without a vector). The embedding model is different: it loads once, at start, and a server without it exits instead of starting, so under Kubernetes or another orchestrator it is restarted rather than left out of service while its liveness probe passes.

For CI and scripted ops, point any command at a server without editing the config: `dmem --endpoint https://memory.example.com recall "x"` (overrides `DM_ENDPOINT`; the token comes from `DM_TOKEN` or the config). `--scope <scope>` (or `DM_WRITE_SCOPE`) stamps the invocation's writes with that scope, for example `dmem --scope room:ops import ./notes`; reads are unchanged. `--scope ''` is a deliberate global write (an empty `DM_WRITE_SCOPE` is not). It needs a 0.3.7 or newer server: against an older one, which would ignore it, the client refuses the write.

## Running the server

`dmem serve` stops gracefully on SIGTERM (what systemd and Kubernetes send) or SIGINT: it stops accepting connections at once, lets in-flight requests finish for up to `DM_SHUTDOWN_GRACE_SECS` (default 15), then flushes and closes every open tenant's vector index and database, and exits 0. Requests still running after the grace are cut off. After the grace, a stop can take up to 7 s more plus the flushes, so give the service manager a stop timeout of at least the grace plus 10 s. In Kubernetes the preStop sleep counts against `terminationGracePeriodSeconds` too: set it to at least preStop + `DM_SHUTDOWN_GRACE_SECS` + 10 (the default 30 fits a 5 s preStop with the default 15 s grace).

Embedding runs outside the tenant lock and is bounded on its own: at most `DM_EMBED_CONCURRENCY` record bodies at once across all tenants (default: half the CPUs, between 1 and 2), and at most `DM_EMBED_QUERY_CONCURRENCY` recall queries (default 2), which have their own permits so a recall never waits behind long saves. Each takes 1-64.

Health endpoints, for probes and monitoring:

- `GET /healthz` (no token): liveness. 200 whenever the process answers.
- `GET /readyz` (no token): readiness. 200 `{"status":"ready","checks":{...}}` when the server should take traffic; 503 with `"failing": [...]` when the IAM database is not open or its file is gone (`iam`), the embedder is the placeholder because the model did not load (`embedder`; only under `DM_REQUIRE_MODEL=0`, detail `degraded: placeholder embedder`), a tenant has no vector index (`vectors`), the data dir cannot be written (`data_dir`), the start-up pass is still running (`startup`), or a stop is in progress (`shutdown`). Without a token each check reads `pass` or `fail`; with the admin token or `DM_METRICS_TOKEN` it carries its detail. No body names a tenant; the server log does, once per change. At start-up the server opens every known tenant that has a database (IAM's active tenants and the `DM_TOKEN_*` tenants) and reports not ready until that is done, so each is judged before traffic. The disk checks are reused for 2 s.
- `GET /metrics`: Prometheus text format, for the admin token or a scrape-only `DM_METRICS_TOKEN` (at least 16 characters; tenant tokens get 403). Series: `dmem_tenant_lock_wait_seconds` and `dmem_tenant_lock_hold_seconds`, `dmem_request_duration_seconds` and `dmem_responses_total` by route, `dmem_embed_seconds` by phase (`before_lock`, `under_lock`), `dmem_embedder_info` (name, and whether it is the placeholder), `dmem_zvec_failures_total` by operation, `dmem_embed_permits` and `dmem_embed_permit_wait_seconds` by pool (`save`, `query`), `dmem_tenants_open`, `dmem_tenants_without_vector_index`, `dmem_requests_in_flight`, `dmem_ready` and `dmem_build_info`. Aggregates only: no label carries a tenant name.

## Security model

The trust boundaries, plainly:

- **Tenant = the hard boundary.** One tenant, one SQLite database, one vector index. Nothing crosses tenants.
- **Scope = the partition inside a tenant**: per-record audience labels, server-stamped writes, grant-filtered reads. Scoped tokens cannot write global; adapter tokens assert a reader's scopes per request. An identity short of full-tenant (restricted reads, or a named write scope) retracts, invalidates and links only its own write scope's records (the global rows, for a restricted reader that writes global); only a full-tenant identity (writes global, reads everything) may write into the `agents/` tree or run the batch `reindex_*` jobs. Backdating (`created_ms`) or raising importance on import is refused only to scoped and adapter tokens: a full-tenant token keeps both when it names a room too, so `dmem --scope room:ops migrate --file rows.jsonl` against a 0.3.8 server imports the records into that room with their original dates. Scope hides content, not every trace of it: edges carry no scope yet, so a refused link can reveal that another scope holds a record with a guessed title. A tenant can also require every write by a full-tenant token to name its scope (`dmem admin policy <tenant> --require-explicit-write-scope true`); such writes then need `dmem --scope <scope>` (`--scope ''` for a deliberate global write), and writes that name none are refused.
- **Agent label = persona protection + attribution**, not a general ACL. The `agents/` tree is isolated per agent; everything else in the tenant is a shared pool. Share a tenant only with mutually-trusted agents/users: anything a member writes can reach every member's context (shared brains share prompt-injection surface too).
- **Transport:** `dmem serve` refuses plain HTTP on a non-loopback bind unless you pass `--allow-insecure-http`. Use `--tls-generate` or bring your own cert. TLS key exchange is post-quantum hybrid, X25519MLKEM768 first, with X25519, P-256 and P-384 for peers without ML-KEM; certificates stay classical. Clients always verify the server certificate against the Mozilla root store built into dmem or the cert you pin; there is no switch to skip it, because every request carries the bearer token. For a self-signed server, pin its cert: start the server with `--tls-san <the name clients use>`, copy the `cert.pem` it writes under `<data>/tls/`, and run `dmem login <url> --ca-cert <cert.pem>`. A server certificate you bring must not itself be a CA (a plain `openssl req -x509` sets CA:TRUE) and must name the host or IP clients use in a subjectAltName; the CN alone is not read. A config that still says `insecure = true` is refused with that recipe.
- **Startup is fail-closed:** an unopenable IAM database refuses to start (revocation would not be enforced) unless you pass `--allow-env-only`; `DM_TOKEN_*` env secrets under 16 characters are rejected outright.
- **Admin token:** on a fresh data dir `dmem serve` mints a root admin token, prints it once and writes it to `<data>/admin.token` (0600). Set `DM_ADMIN_TOKEN` (for example from a Secret) to supply it instead: then no token is ever minted, printed or written, even when the variable is empty. Only its current value is an env admin token: after a rotation, the next start revokes the old value.
- **Unauthenticated routes:** only `/healthz` and `/readyz`, for probes. They reveal liveness and which readiness checks pass or fail, never the reason, a tenant, a path or the version. `/metrics` and the readiness detail need the admin token or `DM_METRICS_TOKEN`.
- **Files:** `dmem serve` runs with umask 077, so the databases, their `-wal`/`-shm` files, the vector index and the TLS key it creates are owner-only; only the generated TLS certificate is world-readable.
- **Tokens:** prefer `dmem login <url>` with `$DM_LOGIN_TOKEN` or the prompt - a positional token lands in shell history and `ps`.
- **Files:** tenant DBs and `iam.db` are created/re-chmodded 0600 on Unix. On Windows there is no ACL tightening yet - rely on the account boundary.
- **Upgrades are signed:** `dmem upgrade` installs a release only when its `.minisig` verifies against a release key compiled into your binary, and refuses otherwise. 0.3.6 replaced that key, so an install on 0.3.5 or older cannot verify 0.3.6: install 0.3.6 once by hand from the release page, and `dmem upgrade` works again from there.
- **The local graph viewer (`dmem ui`) has no auth** by design: keep it on the default loopback bind and reach it over an SSH tunnel. It rejects DNS-name Host headers (rebinding guard); IP-literal and localhost Hosts are allowed.

## Docs

Full documentation is in the [project wiki](https://github.com/wakbijok/dm-lite/wiki): install and first run, wiring each agent, run as a server, run as a client, embedding models and footprint, multitenant admin, persona and governance, migrating from v1, upgrading, and building from source.

License: [Apache License 2.0](LICENSE), with attribution in [NOTICE](NOTICE). See also [CONTRIBUTING](CONTRIBUTING.md), [CODE_OF_CONDUCT](CODE_OF_CONDUCT.md), and [SECURITY](SECURITY.md).
