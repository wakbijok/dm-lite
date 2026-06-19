# dm-lite

**daimon-memory v2** - a small, embedded, typed memory engine for AI agents, with
hybrid recall, behind a `MemoryStore` trait. One binary, embedded now, server +
multitenant next. Same memory model as v1; only the engine is new.

> Staging: `git.wakbijok.uk/daimon/dm-lite`. Design: `daimon-docs/daimon-memory/`
> (SRS, SDS v0.3 Section 2.6), roadmap `daimon-docs/dm-lite/2026-06-19-roadmap.md`.

## Quick start

```bash
cargo build --release
install -m755 target/release/dmem ~/.local/bin/dmem

# save typed memory
dmem log_decision --title "Lock LanceDB" --decision "use LanceDB" --rationale "GA vector + hybrid"
dmem log_lesson   --title "AVX2 gate"   --lesson "the embedder needs AVX2"
dmem remember "Devin is the Windsurf lineage"

# recall
dmem recall lancedb vector
dmem recent
```

## Test it on Devin (or Claude Code)

```bash
dmem bootstrap --devin     # installs SessionStart + UserPromptSubmit hooks into ~/.config/devin/config.json
dmem bootstrap --claude    # or Claude Code (~/.claude/settings.json)
```

Then start a `devin` session: at session start dmem injects persona + recent context;
on each prompt it recalls relevant memory and injects a `<daimon-memory>` block. Hooks
fail open - a memory hiccup never blocks your turn. (Old config backed up to
`config.json.dm-bak`.)

## What it is (M0)

| Piece | Status |
|---|---|
| Typed `Entry` model: kinds, `daimon://` URIs, namespaces | done |
| Guided save tools with per-kind required-field validation | done (`log_decision`/`log_lesson`/`log_incident`/`remember`) |
| `MemoryStore` trait + **SQLite** impl (FTS5 keyword recall, dedup/supersede, close-not-delete) | done |
| Hybrid recall: keyword (FTS5) + **dense vector (zvec) + RRF** | done (vector behind `--features zvec`) |
| CC-compatible hooks + `bootstrap` (Devin, Claude Code) | done |
| Bitemporal store, signal rescoring, SessionEnd nudge, server mode | done (M1, see below) |

The architecture is engine-swappable on purpose: the pure-Rust SQLite store ships by default
(offline, keyword-only, zero models); the **zvec** dense-vector substrate (Wak's choice over
LanceDB) drops in behind the same `MemoryStore`/`Embedder` seams under `--features zvec`.
Source text is canonical; vectors are a rebuildable index.

## Dense vector recall (zvec, optional feature)

The chosen vector substrate is Alibaba **zvec** (in-process, Apache-2.0), behind a feature
so the default build stays pure-Rust, offline, keyword-only.

```bash
# zvec = vector store; fastembed = real bge-small embeddings (semantic recall)
cargo build --release --features zvec,fastembed
cp target/release/dmem ~/.local/bin/dmem
# zvec links a native lib; ship it ALONGSIDE the binary (build.rs rpaths @executable_path):
cp "$(find target -name 'libzvec_c_api.dylib' | head -1)" ~/.local/bin/
dmem status      # -> recall : hybrid: SQLite FTS + zvec vector (RRF)
```

With `--features zvec`, every save also writes an embedding to a zvec collection
(database-per-tenant at `<data>/vectors/<tenant>`); recall fuses SQLite FTS (keyword) +
zvec vector via RRF. Add `fastembed` for real semantics (without it the offline
`HashEmbedder` placeholder is used and recall stays keyword-equivalent).

**Honest status of the zvec path:**
- zvec store + search + cross-process persistence: working, unit-tested (10 tests green
  with `--features zvec,fastembed`). zvec caps the primary key at 64 bytes and rejects `:`
  `/`, so the daimon:// URI is hashed (128-bit FNV) into the PK and the real URI is stored
  in a string field, read back on search (`long_daimon_uri_roundtrips` covers this).
- The binary needs `libzvec_c_api.{dylib,so}` at runtime (zvec is a C++ core via FFI). The
  first build is heavy (~2 min: pulls the zvec C++ tree + arrow/rocksdb/protobuf). On
  failure to load, recall falls back to keyword-only and `status` says so.
- **Real semantic recall is live** with `fastembed`: bge-small-en-v1.5 (384-d, ONNX via
  `ort`) embeds every save and every query. Verified end-to-end — recalling *"what crashed
  our data store"* (zero shared keywords) ranks *"the Postgres database server was
  OOM-killed during migration"* first via the vector half alone (`kw=0 vec=3`). The model
  downloads once to `.fastembed_cache/` on first use; if it can't load, dmem logs the
  fallback and uses the `HashEmbedder` placeholder so saves/recall never block.

## M1 (complete)

All five M1 items shipped, each behind the existing seams:

1. **Real embedder for semantic recall** - bge-small-en-v1.5 (fastembed/ONNX) behind the
   `Embedder` trait (`--features zvec,fastembed`). Verified: `kw=0 vec=3` finds a
   semantically-related memory with no shared words.
2. **Full bitemporal** - two independent time axes: valid time (true-in-world) and system
   time (recorded-at). The store is append-only (supersede closes the prior version in
   system time, never deletes). As-of queries reconstruct any past slice.
   - `dmem recall "<q>" --as-of <epoch_ms>` - recall the store as it existed then.
   - `dmem history <uri>` - the full version lineage (newest first).
3. **Runtime-signal rescoring** - a `signals` sidecar (access/recency/importance) applied as
   a modest, clamped (<=1.25x) multiplier AFTER RRF, so it only reorders near-ties and never
   overrides relevance. Bumped best-effort on every recall.
4. **Save-discipline nudges** - a `SessionEnd` hook surfaces uncaptured work (fail-open;
   nudges only when nothing was saved or the last save is stale). Installed by `bootstrap`.
5. **Server mode** - `--features server`: an axum + tokio network API over the database-
   per-tenant store, with multi-token bearer -> tenant auth. See below.

Typed kinds with no guided tool yet (`resource_summary`, `persona`, `protocol`) and the
MCP surface beyond the core four are easy follow-ons.

## Server mode (`--features server`, optional)

A small axum + tokio HTTP API over the per-tenant store. Strictly feature-gated, so the
default embedded build pulls no tokio/axum.

```bash
cargo build --release --features server   # add zvec,fastembed too for semantic recall
# auth: each DM_TOKEN_<TENANT> env var registers a bearer token -> that tenant
export DM_TOKEN_ACME=<secret>
dmem serve --addr 127.0.0.1:8077          # Ctrl-C for graceful shutdown
```

Routes (all but `/healthz` require `Authorization: Bearer <token>`; the token selects the
tenant per request, over `config::db_path`):

| Method + path | body | returns |
|---|---|---|
| `GET /healthz` | - | `{"status":"ok"}` (open, no auth) |
| `POST /recall` | `{query, limit?}` | array of records |
| `POST /remember` | `{text, namespace?}` | `{uri}` |
| `POST /log_decision` | `{title, context?, decision, rationale?, namespace?}` | `{uri}` |
| `POST /add_reminder` | `{title, text, namespace?}` | `{uri}` |

Auth goes through an `Authenticator` seam (bearer now; JWT can drop in later without
touching handlers). At this scale (tens-to-~100 users over per-tenant SQLite) the server
opens the tenant store per request; a per-tenant cache is a noted follow-on.

## Layout

```
src/entry.rs      typed model (Kind, Entry, daimon:// URI, bitemporal fields)
src/store.rs      MemoryStore trait (the engine seam): put/recall/recall_as_of/history
src/sqlite.rs     SQLite impl: FTS5 recall, append-only bitemporal store, v0->v1 migration, signals
src/tools.rs      guided typed save tools + recall + signal rescoring (daimon's layer)
src/embedder.rs   Embedder trait: HashEmbedder (default) / FastEmbedder (bge-small, fastembed)
src/zvec_index.rs zvec dense-vector index (feature zvec)
src/render.rs     <daimon-memory> / <daimon-persona> / save-nudge context blocks
src/hooks.rs      session_start / user_prompt_submit / session_end handlers
src/bootstrap.rs  install hooks into agent configs
src/server.rs     axum network API + bearer->tenant auth (feature server)
src/config.rs     data dir + database-per-tenant paths
poc/              the throwaway Node PoC that proved the seam
```

License: MIT.
