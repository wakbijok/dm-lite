# M1 Implementation Plan — daimon-memory v2 (`dmem`)

> **For agentic workers:** TDD per task (failing test → run → implement → run → commit). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Complete M1 for `~/Projects/dm-lite`: harden the semantic test (Fix A), fix the MCP server name (Fix B), then land full bitemporal storage (M1.2), runtime-signal rescoring (M1.3), save-discipline nudges (M1.4), and feature-gated server mode (M1.5).

**Architecture:** Entry gains two-axis temporal fields; SQLite becomes append-only with `(uri, system_to_ms)` versioning; `Memory` stays the single chokepoint; signals bolt on as a sidecar table applied as a post-RRF multiplier; server mode is axum+tokio behind `--features server` with multi-token bearer→tenant over the existing database-per-tenant store.

**Tech Stack:** rusqlite (bundled SQLite+FTS5), clap, serde, anyhow, dirs; optional zvec, fastembed; new optional axum 0.7 + tokio 1 under `server` feature.

**Locked design decisions (from Wak, 2026-06-20):** server auth = multi-token bearer→tenant (`DM_TOKEN_<tenant>`); server stack = feature-gated axum/tokio; bitemporal = full system + valid time.

---

## Sequence (dependency order)
Fix B (trivial) → Fix A → M1.2 bitemporal (schema/model first) → M1.3 signal rescoring → M1.4 save-discipline nudges → M1.5 server mode (built on the finalized Memory API).

## Task list
- **Task 0** — branch `m1-bitemporal-signals-server`, confirm baseline 7 tests green.
- **Fix B (B1)** — MCP serverInfo name `dm` → `dmem` (`src/mcp.rs:117`), via testable `server_info()` fn.
- **Fix A (A1)** — gated `#[cfg(feature="fastembed")]` test asserting FastEmbedder ranks a related pair over an unrelated pair (cosine).
- **M2.1** — add bitemporal fields to `Entry` (`valid_from_ms`, repurposed `valid_to_ms`, `system_from_ms`, `system_to_ms`); fix all constructors.
- **M2.2** — SQLite schema v1 + `PRAGMA user_version` v0→v1 migration (append-only, transactional, idempotent via `table_info`).
- **M2.3** — rewrite `put()` to close-in-system-time + insert new version (append-only); current-slice predicate replaces `valid_to_ms IS NULL` in get/get_by_id/recent/by_kind.
- **M2.4** — `MemoryStore::recall_as_of` + `history` (as-of = linear scan; history = full lineage).
- **M2.5** — `Memory::recall_as_of`/`history` + CLI `dmem recall --as-of <ms>` and `dmem history <uri>`.
- **M2.6** — confirm zvec only ever holds the current version; build gates under features.
- **M3.1** — `signals` sidecar table + `bump_signal`/`read_signal`/`read_signals`.
- **M3.2** — pure deterministic `signal_boost(importance, access, last, now)` clamped ≤1.25×.
- **M3.3** — apply boost after RRF (and keyword-only path) + bump-on-recall; test that relevance still wins over frequency.
- **M4.1** — `HookCmd::SessionEnd` + `session_end()` handler + `should_nudge` heuristic (fail-open).
- **M4.2** — `render::render_nudge()`.
- **M4.3** — bootstrap installs SessionEnd hook into Devin + Claude configs.
- **M5.1** — Cargo: optional axum+tokio under `server` feature; gate-check default build pulls neither.
- **M5.2** — `Authenticator` trait + `BearerAuth::from_env()` (`DM_TOKEN_<tenant>` → token map), pure-tested.
- **M5.3** — axum routes mirroring the tool surface (recall/remember/log_decision/add_reminder/healthz); add `Memory::open_tenant`.
- **M5.4** — `dmem serve --addr` + tokio runtime + graceful shutdown.
- **M5.5** — README update + final gate matrix.

## Risks
- **Bitemporal migration is irreversible per db file** — transactional, `user_version`-guarded, idempotent via `table_info`, fail-closed. (Low real impact: dm-lite has no production data yet; v1 daimon-memory is a separate Postgres system.)
- **axum/tokio dep weight** — strictly gated; `cargo tree | grep -E 'axum|tokio'` must be empty on default.
- **Signals overpowering relevance** — clamped ≤1.25× post-RRF tiebreaker; test asserts a strong match isn't flipped by access frequency.
- **As-of bypasses FTS/vectors by design** — linear scan, fine at tenant scale.
- **Server tenant env race** — `Memory::open_tenant` avoids mutating process-global `DM_TENANT` under tokio concurrency; open-per-request.

(Full step-by-step detail is the canonical plan from the Plan agent; this file is the executable checklist.)
