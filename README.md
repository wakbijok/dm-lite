# dm-lite

**daimon-memory v2** - a small, embedded, typed memory engine for AI agents, with
hybrid recall, behind a `MemoryStore` trait. One binary, embedded now, server +
multitenant next. Same memory model as v1; only the engine is new.

> Staging: `git.wakbijok.uk/daimon/dm-lite`. Design: `daimon-docs/daimon-memory/`
> (SRS, SDS v0.3 Section 2.6), roadmap `daimon-docs/dm-lite/2026-06-19-roadmap.md`.

## Quick start

```bash
cargo build --release
install -m755 target/release/dm ~/.local/bin/dm

# save typed memory
dm log_decision --title "Lock LanceDB" --decision "use LanceDB" --rationale "GA vector + hybrid"
dm log_lesson   --title "AVX2 gate"   --lesson "the embedder needs AVX2"
dm remember "Devin is the Windsurf lineage"

# recall
dm recall lancedb vector
dm recent
```

## Test it on Devin (or Claude Code)

```bash
dm bootstrap --devin     # installs SessionStart + UserPromptSubmit hooks into ~/.config/devin/config.json
dm bootstrap --claude    # or Claude Code (~/.claude/settings.json)
```

Then start a `devin` session: at session start dm injects persona + recent context;
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
