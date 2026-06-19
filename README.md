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
| Hybrid recall: keyword now; **dense vector + RRF** next | keyword done; vector pending |
| CC-compatible hooks + `bootstrap` (Devin, Claude Code) | done |
| **LanceDB** impl (GA vector + built-in hybrid) behind the trait | next |
| Server mode + database-per-tenant; MCP tool surface | next |

The architecture is engine-swappable on purpose: SQLite ships M0 today (offline,
keyword-only, zero models); **LanceDB** is the locked production engine that drops in
behind the same trait for dense vector recall. Source text is canonical; vectors are a
rebuildable index. Embedder is a commodity swap behind an `Embedder` trait.

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

## Next (M1, toward complete v2)

In priority order, each behind the existing seams (no model change):

1. ~~**Real embedder for semantic recall**~~ - **done** (bge-small via fastembed/ONNX behind
   the `Embedder` trait; `--features zvec,fastembed`). The zvec store + RRF fusion were
   already wired; this swapped the placeholder for a real model, unlocking true semantic
   recall (verified: `kw=0 vec=3` finds a semantically-related memory with no shared words).
2. **Bitemporal** - replace the soft-close (`valid_to_ms`) with a system+valid-time
   versions model; as-of queries.
3. **Runtime-signal rescoring** - access/importance/recency/maturity sidecar; reweight
   recall modestly.
4. **Save-discipline nudges** - SessionEnd/Stop hook that surfaces uncaptured decisions.
5. **Server mode** - the same binary behind a network API; per-request tenant JWT over
   the database-per-tenant store (`config::db_path`).

Typed kinds with no guided tool yet (`resource_summary`, `persona`, `protocol`) and the
MCP surface beyond the core four are easy follow-ons.

## Layout

```
src/entry.rs      typed model (Kind, Entry, daimon:// URI)
src/store.rs      MemoryStore trait (the engine seam)
src/sqlite.rs     SQLite impl: FTS5 recall, dedup/supersede
src/tools.rs      guided typed save tools + recall (daimon's layer)
src/render.rs     <daimon-memory> / <daimon-persona> context blocks
src/hooks.rs      session_start / user_prompt_submit handlers
src/bootstrap.rs  install hooks into agent configs
src/config.rs     data dir + database-per-tenant paths
poc/              the throwaway Node PoC that proved the seam
```

License: MIT.
