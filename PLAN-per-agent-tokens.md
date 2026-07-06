# feat/per-agent-tokens — implementation plan

Decision refs: daimon://resources/dm-lite/decision/dmem-per-agent-identity-auth-token-per-agent-not-query-param
Problem: every MCP client receives the same persona ("I am Izu") as binding
instructions — identity bleed for shesta/devin. Fix: token carries agent
identity; persona served per-agent; protocols/house-rules stay shared.

## Constraints (hard)

- Memory stays SHARED: same tenant DB for all workspace agents. Do NOT use
  tenant-per-agent (that would silo memory).
- Backward compatible: agent-less tokens keep working exactly as today
  (serve shared/protocol records + ALL legacy persona until migration).
- No breaking change to embedded/stdio mode defaults (single-user laptop use).
- NEVER log or echo token plaintext; only hashes (existing convention).

## Changes

1. **iam.rs** — tokens table: add nullable `agent TEXT` column (ALTER TABLE,
   idempotent migration). `Identity { tenant, is_admin, agent: Option<String> }`.
   Token create/list admin ops accept optional agent label.

2. **server.rs** — env token form `DM_TOKEN_<TENANT>__<AGENT>=secret`
   (double underscore separator; keep `DM_TOKEN_<TENANT>` = agent-less).
   `Authenticator::tenant_for` → return identity incl. agent (rename or add
   `identity_for`). Thread agent through request context to the MCP layer.

3. **Persona selection** (render.rs + callers in mcp.rs/bootstrap.rs/hooks.rs):
   - Agent personas live in namespace `agents/<agent>/persona`.
   - Shared governance (protocols, house rules/boundaries) live where they do
     today (Kind::Persona outside agents/ namespaces) and go to EVERYONE.
   - With agent identity: instructions = shared governance + that agent's
     persona records only. Without agent: current behaviour unchanged.
   - render_soul/render_session/render_instructions take the filtered set —
     prefer filtering at the query (store) level, not in render.

4. **Attribution** — when an authenticated agent writes (remember/log_*),
   stamp author=<agent> into the entry meta/source field if not already set.

5. **Tests** — unit: iam agent column round-trip + env-token parsing (incl.
   `__` edge cases); render: agent gets own persona + shared governance, not
   another agent's; agent-less token sees legacy behaviour; attribution stamp.

6. **Docs** — README/CHANGELOG entry. No AI-vendor words in commits/docs.
   Commits: author Muhammad Arif <arifchehusin@gmail.com>, sign body "Signed: Izu".

## Explicitly OUT of scope tonight (morning work, Wak awake)

- Deploying to dmem-vps (production for all bots).
- Editing/migrating LIVE persona records on dmem-vps (content split into
  agents/izu/persona + neutral shared house rules) — draft the new record
  BODIES into migration-notes.md in the repo, but do not touch prod.
- Bridge .env/MCP config changes (needs new tokens minted on the VPS).

## Definition of done (tonight)

- cargo build + cargo test green, fmt + clippy clean.
- Branch feat/per-agent-tokens pushed to staging remote
  (git.wakbijok.uk/daimon/dm-lite) in reviewable commits.
- migration-notes.md drafted (record bodies + deploy runbook for morning).

Signed: Izu, 06-07-2026
