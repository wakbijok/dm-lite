# Scope-exact engine (0.3.7, E1-E7)

Status: implemented in 0.3.7 (unreleased). Built on branch `feat/scope-exact` from main `6115bc4` and merged locally on 28-09-2026. The spec is items E1-E7 of the 0.3.7 engine follow-up to the workspace memory partition review. E8 and E9 were proposed in the same review and are not implemented (see the last section).

This document describes the behaviour as built. The user-facing summary is in `CHANGELOG.md` under 0.3.7.

## Problem

The scope primitive (0.3.2) gave every record one audience label and filtered reads by it. Up to 0.3.6, some engine paths still treated a uri as one record:

- `forget` and `invalidate` closed every scope's live rows of a uri. The guard in front of them looked only at the newest current row across all scopes. A room could close another room's record, or the global one, when it happened to have saved last. It was refused when an invisible scope had saved last.
- `get` and `scope_visible` judged a uri by its newest live row across all scopes. A room that saved a record with a global record's kind, namespace and title (the same uri) hid the global record from every other room. The vector index held one vector per uri, so the room's text also replaced the global record's vector.
- A uri with no live row counted as visible to everyone. A retracted room record's uri (its title, in slug form) could then show through a dangling edge.
- Scoped reads over-fetched four times the limit and filtered afterwards. Heavy churn in an invisible scope could push the reader's own rows and the global rows out of the window.
- A scoped writer could add or remove any edge between records it could see, two global records included, with free text as the label.
- `import_record_at` (the `/import` form with `created_ms` or `importance`) ignored the write scope and the `agents/` tree guard, and saved GLOBAL.
- A full-tenant token could not say which scope a write was for. The server ignored `X-DM-Write-Scope` from such a token, so every write it made landed in global.

## Terms

- `ws`: the identity's effective write scope for this request. `''` is global. None means the identity cannot write.
- `R`: the read set. Global is always readable. None means the whole tenant.
- live: `system_to_ms IS NULL`. current: live and valid now.
- **Full-tenant identity:** writes global (`ws = ''`) and reads the whole tenant (`R` = None). Code: `LocalMemory::full_tenant()`.
- **Write-restricted identity:** every other identity. Code: `write_restricted()`.

How requests map to identities:

| Request | `ws` | `R` | Full tenant |
|---|---|---|---|
| Scope-unbound token, no `X-DM-Write-Scope` | `''` | whole tenant | yes |
| Scope-unbound token, `X-DM-Write-Scope: ''` | `''` | whole tenant | yes |
| Scope-unbound token, `X-DM-Write-Scope: room:x` | `room:x` | whole tenant | no |
| Scope-unbound token, no header, tenant has the E6 policy | none (refused, policy message) | whole tenant | no |
| Adapter, `X-DM-Write-Scope: s`, `X-DM-Scopes: A,B` | `s` | A, B, s | no |
| Adapter, `X-DM-Write-Scope: ''` | `''` | as asserted | no (read-restricted global writer) |
| Adapter, no `X-DM-Write-Scope` | none (read-only) | as asserted | no |
| Capped adapter, write scope outside the cap, or `''` | none (refused, with reason) | asserted, cut to the cap | no |
| Plain scoped token | its `scope_write` (None = read-only) | `scope_read` plus `scope_write` | no |
| Embedded CLI, no `DM_SCOPE` | `''`, or `--scope` | whole tenant | yes, unless `--scope` names a scope |
| Embedded CLI, `DM_SCOPE=s` | `s` (another `--scope` is refused) | `s` | no |

A **read-restricted global writer** (an adapter promoting with `''`, or a scoped token minted with a `''` write scope) writes global but does not read the whole tenant. Up to 0.3.6 the engine took any `''` writer for a full-tenant one. In 0.3.7 such an identity is write-restricted, and global is its own scope: its retractions, invalidations and edges are bounded to the global rows. It cannot use the E7 levers or write into the `agents/` tree.

## As built

### E1: scope-exact forget and invalidate

- `SqliteStore::forget_in(uri, scope)` and `invalidate_in(uri, valid_to, scope)`. `None` selects every scope's live rows (full-tenant writer, unchanged). `Some(ws)` selects only `uri = ?1 AND scope = ?2`.
- `LocalMemory::mutation_scope()` is None for a full-tenant identity, else `Some(ws)`. A write-denied identity is refused here.
- `guard_mutation_target` (via `SqliteStore::mutation_rows`) needs at least one live row of the uri in the selection. For a write-restricted identity, a miss answers `record not found in this identity's write scope`. Every selected row must also pass the agent-tree gate: an agent cannot retract another agent's `agents/<b>/...` rows. It does not matter which scope saved last.
- The returned count covers the selected rows only, so a scoped caller learns nothing about other scopes' rows.
- Graph cascade: edges of the uri are deleted, in the same transaction, only when the mutation closed something AND no live row of the uri is left in any scope (`has_live_row`).
- Vectors: `mutation_slots` records the slots the mutation touches and the body each one embeds now, before the mutation runs. `resync_vector_slots` runs after it: a slot whose scope has no live row left is removed, a slot whose newest live row now has another body is re-embedded, and a slot that already embeds the same body is left alone. An invalidation's remainder is a copy of the cut segment, so the usual cut costs no embedding. Other scopes' slots are never touched.

### E2: dead-uri visibility and the invalidate cascade

- `scope_visible(uri)`: a visible live row makes the uri visible. Live rows the reader cannot see keep it invisible. A uri with no live row is judged by its newest row ever, closed rows included, on both gates (scope and agent tree). Only a uri with no row at all stays visible, which keeps the bridging behaviour for nodes that never existed.
- `invalidate_in` deletes the uri's edges when the cut leaves no live row in any scope (a cut at or before the start of every live segment), as `forget` does. `prune_dangling_edges` is unchanged.

### E3: exact limits for gated readers

- `visibility_sql(prefix, first)` renders the reader gates as one SQL fragment: `(scope = '' OR scope IN (SELECT value FROM json_each(?n)))` with the read set bound as one JSON array, plus the agent-tree clause (`agent_tree_sql`). An unrestricted reader gets `1`.
- The fragment is used by `get`, `history`, `recent`, the keyword channel of recall, as-of recall, `by_kind`, `by_kind_for_agent` (the persona boot layer), `latest_save_ms` and `scope_visible`. `LIMIT` counts visible rows, and the four-times over-fetch is gone.
- The persona route's agent-tree match is case-insensitive, like every other agent-tree check.

### E4: the newest visible row wins, and one vector slot per record

- `get(uri)` picks the newest current row among the rows the reader may see. An invisible newer row no longer hides a visible older one. `scope_visible` follows the same rule.
- Vector index: one slot per record, keyed by (uri, scope). `zvec_index::slot_of(uri, '')` is the historical bare-uri FNV key, so global vectors written before 0.3.7 are already in the right place. A scoped slot id is SHA-256 of `s<len>:<scope><uri>`, cut to 128 bits (32 hex characters). A crafted title cannot collide with another record's slot.
- `save_entry` upserts the record's own slot, so a room save never overwrites the global record's vector.
- A vector hit is resolved to the one record its slot embeds (`resolve_vector_hits`) and hydrated as that record (`get_in_scope`). A slot the reader cannot see, or a stale one with no live row left, is dropped before the floor and fusion, so it never ranks a record the reader gets.
- Hybrid fusion ranks records, keyed by `record_key(uri, scope)`. Then `one_per_uri` keeps one hit per uri at the best rank any of its copies reached. The kept hit is the copy in the reader's own write scope if it made the ranking, else the newest.
- Vector crowd-out: a reader behind a gate (scopes or an agent view) searches again with k times 4 until it has `pool` visible records, the index runs out, or k reaches 1024 slots. An ungated reader searches once, as before.
- Access signals are per record. `signal_key(uri, '')` is the bare uri, the key every signal row had before, so rows stored by 0.3.6 carry over as the global record's. A scoped record's key is `s<len>:<scope><uri>`. A uri always starts with `daimon://`, so the two forms cannot clash. There is no schema change.

### E5: edge ownership in the engine

- `guard_edge_endpoints(from, to)`: for a write-restricted or agent-labelled identity, both endpoints must be visible (the E4 `get`). An invisible endpoint reads as not found. A write-restricted identity also needs one endpoint that is wholly its own (`wholly_in_scope`): at least one live row, and every live row in `ws`. This test ignores the reader gates, so a holder the caller cannot see still makes the uri shared.
- `guard_rel`: a write-restricted identity's `link` needs `rel` matching `^[a-z][a-z0-9_-]{0,31}$` (`valid_rel`). `unlink` accepts any rel, so legacy free-text edges can still be removed. Full-tenant writers keep any label.
- Refusals answer 400 with the rule, for example `invalid rel: ...` or `an edge needs one endpoint that only this identity's write scope holds`.

### E6: explicit write scope per tenant (opt-in)

- `iam.db`: `tenants.require_explicit_write_scope INTEGER NOT NULL DEFAULT 0`, added at open when missing. Set it with `dmem admin policy <tenant> --require-explicit-write-scope true|false` (admin route `POST /admin/tenant_policy`). The flag defaults to false. `dmem admin list` shows `explicit write scope` for a tenant that has it. The server reads the policy on every request, so a change applies at once. With `--allow-env-only` (no IAM) no policy is enforced.
- `scope_binding`: on a policy tenant, a scope-unbound token without `X-DM-Write-Scope` gets no write scope, with the refusal reason `this tenant requires an explicit write scope`. Every write path answers 400 with that message and stores nothing: the typed saves, `/import`, `/forget`, `/invalidate`, `/link`, `/unlink` and the batch `reindex_*` routes, over REST and MCP. Reads are unchanged, and so are scoped and adapter tokens.
- On every tenant, a scope-unbound token's `X-DM-Write-Scope` now stamps its writes with the named scope (`''` = global). Before, the header was ignored for such tokens.
- `X-DM-Write-Scope` is read as absent, named or unusable (`write_scope_header`). An unusable value (outside printable ASCII, with a comma, or the header repeated) answers 400 on every route and token type. It never reads as absent, which would land the write in the token's default scope.
- CLI and client: the global `--scope <scope>` flag, or `DM_WRITE_SCOPE`. `--scope` is kept in process (`config::set_cli_write_scope`), so child processes do not inherit it. Only `--scope ''` names a global write; an empty `DM_WRITE_SCOPE` counts as unset. The remote client sends the header on every memory request. Before the first write that carries it, the client asks `GET /capabilities` once. It refuses the write when the server is older than 0.3.7 (404) or when the token is a plain scope-bound one (`write_scope_header: false`). It then checks the `scope` echoed in every save reply, and a save that landed in another scope is reported as an error. The embedded engine stamps the scope itself, and a `DM_SCOPE` session refuses a different one.

### E7: `import_record_at` goes through the guards

- `import_record_at` runs `guard_agent_namespace`, then stamps `effective_write_scope()`, so a write-denied identity is refused.
- A write-restricted identity is refused `created_ms` (400 `created_ms is set by the operator`) and any `importance` above the kind's default (400 `importance is set by the operator`). A lower importance is kept. The decision was to refuse rather than silently ignore.
- A full-tenant identity is unchanged: it can backdate and set any importance. On a policy tenant it needs `X-DM-Write-Scope: ''` (`dmem --scope '' migrate ...`).

### Beyond the spec (review follow-up)

- The read-restricted global writer is bounded as described under Terms.
- A capped adapter token's cap also bounds its write scope: a scope outside the cap, or `''`, leaves the request read-only, and its writes answer 400 with the reason. Promotion to global needs an uncapped adapter token.
- `reindex_links` and `reindex_mentions` need a full-tenant identity writing global, checked in the engine (`guard_batch_graph_maintenance`). This covers the MCP tool, which reaches the engine directly.
- `latest_save_ms` returns the newest row the reader may see. Before, a scoped reader could watch when scopes it cannot see were written.
- Engine guard refusals are a typed error (`tools::Refusal`). The server answers them with 400 and the rule's message on every route, and MCP tool errors carry the same message. Other errors keep their generic 500 body.

## Wire compatibility

| Change | 0.3.3 / 0.3.6 client on a 0.3.7 server | 0.3.7 client on an older server |
|---|---|---|
| `scope` field in save replies | ignored | absent; checked only with `--scope`, which the probe refuses first |
| `GET /capabilities` | never called | 404, so `--scope` writes are refused before sending |
| `X-DM-Write-Scope` honoured for full-tenant tokens | never sent | the old server ignores it; the client refuses to rely on that |
| Unusable `X-DM-Write-Scope`: 400 | never sent | the client validates before sending |
| E6 policy tenant: 400 on unnamed writes | writes refused on that tenant only (deliberate) | `dmem admin policy` reports that the server needs upgrading |
| E7 refusals for write-restricted identities | 400 instead of a global save (deliberate) | not applicable |
| Guard refusals: 400 instead of 500 | same failure, now with a reason | not applicable |

A client without `--scope` works against either server version.

## Storage and migration

- **SQLite `entries`, `edges`, `signals`:** no schema change. Signals keep the bare-uri key for global records (see E4). A test reads a 0.3.6-form signal row back as the global record's.
- **`iam.db`:** one added column, `tenants.require_explicit_write_scope`, created at open (`ALTER TABLE ... DEFAULT 0`). An older dmem opening the file ignores it. Test: `opens_a_0_3_6_iam_db_and_adds_the_policy_column`.
- **Vector index:** re-keyed once per tenant. A tenant needs it when `vectors/<tenant>` exists next to `tenants/<tenant>.db` and has no `<tenant>.scope-slots` marker (`tenants_pending_slot_migration`).
  - Work list: every uri a non-global scope has ever held a row of (`uris_with_scoped_rows`). For each one, the bare-uri slot is rebuilt from the newest live global row, or removed when there is none, and every scope with a live row gets its own slot from its newest live row. Records only ever saved globally are untouched.
  - Cost: one embedding per scoped record, about half a second each with the default embedder on a small server.
  - `dmem serve` starts it on a background thread while it is already listening (`migrate_pending_vector_indexes`), one tenant after another. The tenant cache locks per tenant (`slot_for`, `memory_for`) and opens run on the blocking pool. A request for a tenant being migrated waits for it, while other tenants and `/healthz` are served. The embedded CLI migrates on first open.
  - It is resumable: the last finished uri is checkpointed in `<tenant>.scope-slots.progress` (written to a temporary file, then renamed), and the work list is sorted by uri. The progress file is deleted and the marker written when the migration completes.
  - Tests: `opens_a_pre_slot_store_and_migrates_its_vectors`, `pending_vector_indexes_migrate_at_start_up_and_block_only_their_own_tenant` (a 0.3.6-format tenant), and `an_interrupted_slot_migration_resumes_after_its_checkpoint`.

### Rollout

1. Upgrade the servers first (dmem.vps, khd-ai), then the clients. A 0.3.7 client refuses `--scope` against an older server.
2. On the first start, watch the log for `re-keying the vector index per scope` and `vector index ready` for each tenant. A client with a short timeout may fail its first request to a tenant that is still migrating.
3. Turn on the E6 policy for a tenant (`workspace`) only after every client that writes to it runs 0.3.7 and names its scope. Older clients can still read a policy tenant but can no longer write to it.
4. Rollback: an older dmem writes scoped saves into the global vector slot again and does not see the per-scope slots, and nothing detects that afterwards. After running an older dmem on the same data, delete each `<tenant>.scope-slots` marker before starting 0.3.7 again, so the migration re-runs. The same step belongs in the dmem.vps and khd-ai rollback runbooks.

## daimon-hub partition tests against this build

`tests/test_partition_integration.py` was run from `p1/memory-scope` with a PATH shim pointing at the 0.3.7 release binary (dist features). Nothing in daimon-hub was changed. Result: 16 passed, 2 failed, 1 xfailed.

- `test_operator_forget_does_not_close_an_invisible_rooms_record` (strict xfail, E1) now XPASSes. Its strict mark turns that into a failure, which is the signal to remove the mark.
- `test_room_save_does_not_hide_a_global_record_elsewhere` (strict xfail, E4) still XFAILs, and does so on 0.3.6 too. Its query, `zzh shared runbook`, is the record's title, but the body is `operator curated`. That query never clears the recall floor on either version (top cosine 0.525, below 0.62), not even before the room saves. With `DM_RECALL_FLOOR=0` the test passes on both versions, so as written it cannot tell them apart. In a scratch copy that queries the body (`operator curated`), it XFAILs on 0.3.6 and XPASSes on 0.3.7. On 0.3.6, the room's save overwrote the global vector and the global record fell below the floor. The E4 behaviour is correct; the test's query needs to change.
- `test_m2_restricted_forget_never_reaches_a_global_record` fails on 0.3.7. Its precondition asserts that the room's `/history` page with limit 5 shows only room rows. That depends on the truncation E3 removed: the page now also shows the global row. The test's actual checks (403, and the global row stays live) still hold. The precondition needs updating.

Consequences for the daimon-hub proxy (spec follow-up, once 0.3.7 runs on both upstreams):

- A capped adapter token can no longer promote with `X-DM-Write-Scope: ''`. Promotion needs an uncapped adapter token.
- A read-restricted `''` writer is bounded to the global rows, and cannot use the E7 levers or the `agents/` tree. The relaxed proxy rules should expect these 400s.
- The proxy's `/history` ownership lookups (`_owns`, `owned_by`, `survives_cut`) can be deleted. The engine now does that work in E1 and E5.

## Known limitations

- **E5 existence oracle.** The ownership test counts every live row of the uri, including rows in scopes the caller cannot see. A refused link therefore tells the caller that another scope holds a record with that exact kind, namespace and title (existence only, never content). Ownership is also checked only at link time: if another scope later saves the same uri, that scope then sees the edge. E9 fixes both.
- **Vector re-search limit.** The re-search for gated readers stops at 1024 slots. A reader behind heavier invisible churn can still lose vector hits. A filterable scope field in the vector index would remove the limit.
- **`latest_save_ms` scan.** For a scoped reader, the query walks `idx_entries_sysfrom` from the newest row until it finds a visible one. It can scan far when the reader sees very few recent rows, and it runs on every prompt. Benchmark it on a large store before release.
- **Stale slot marker.** Nothing detects a downgrade followed by a re-upgrade (see Rollout). Detecting it automatically would need a high-water mark written on each save, which is a schema change.

## E8 and E9 (not implemented)

What 0.3.7 already provides for them:

- **E8, global-only boot layer for scoped readers:** `full_tenant()` and `write_restricted()` turn the refusal of `persona`, `protocol` and `skill` writes into a one-line guard in `save_valid` and `import_record_at`. `visibility_sql` already feeds `by_kind_for_agent`. Serving the global rows only needs a variant of it that admits only `scope = ''`.
- **E9, edges that carry their creator's scope:** `wholly_in_scope`, `visibility_sql` and the injective `s<len>:` key encoding (used by signals and vector slots) can be reused for `edges.scope` and its migration (existing rows `''`). E9 removes the E5 existence oracle and the link-time-only limitation.

## Tests

Engine and server tests added for E1-E7 and the review follow-up (both `cargo test --features dist` and plain `cargo test`):

- E1: `scoped_forget_is_scope_exact_whoever_saved_last`, `scoped_forget_closes_only_its_own_scope_and_keeps_the_graph`, `scoped_forget_of_the_last_live_row_cascades_edges`, `full_tenant_forget_is_unchanged`, `full_tenant_forget_closes_every_scope_and_cascades`, `scoped_forget_without_own_rows_is_refused`, `scoped_forget_retracts_an_invalidated_own_record`, `scoped_invalidate_is_scope_exact`, `scoped_invalidate_cuts_only_its_own_scope`, `scoped_invalidate_without_own_rows_is_refused_and_full_tenant_is_unchanged`, `full_tenant_invalidate_cuts_every_scope`, `mutation_rows_counts_the_selection_and_checks_the_agent_tree`, `scoped_forget_over_http_is_scope_exact`. Vector slots: `scoped_forget_drops_only_its_own_slot_and_the_last_row_drops_every_slot`, `scoped_forget_of_a_room_only_record_drops_its_vector`, `full_tenant_forget_drops_every_scopes_slot`, `invalidate_re_embeds_only_when_the_slot_text_changes`.
- E2: `dead_uri_visibility_follows_its_newest_row`, `dead_uri_visibility_applies_the_agent_tree_gate`, `dangling_edge_to_a_dead_room_uri_is_hidden_from_other_rooms`, `invalidate_that_leaves_no_live_row_cascades_edges`, `invalidate_leaving_a_live_row_keeps_edges`, `invalidate_that_leaves_no_live_row_drops_the_vector`, `room_invalidate_before_start_cascades_and_other_rooms_never_see_it`.
- E3: `history_limit_is_exact_under_heavy_invisible_churn` (810 invisible versions), `list_reads_limit_is_exact_under_invisible_churn`, `sql_agent_tree_gate_matches_the_row_gate`, `latest_save_ms_is_gated_by_the_readers_view`.
- E4: `get_and_scope_visible_pick_the_newest_visible_row`, `get_in_scope_hydrates_exactly_that_record`, `room_save_does_not_hide_a_global_record_elsewhere`, `room_save_does_not_hide_a_global_record_from_graph_riders`, `room_save_does_not_hide_a_global_record_over_http`, `room_save_keeps_the_global_vector_and_hybrid_recall_finds_it`, `scopes_of_one_uri_hold_separate_slots`, `global_slot_is_the_historical_uri_key`, `vector_hits_resolve_to_their_own_record_and_invisible_slots_drop`, `invisible_slots_do_not_crowd_a_gated_readers_vector_channel`, `recall_gives_one_hit_per_uri_to_a_reader_that_sees_several_copies`, `hybrid_recall_gives_one_hit_per_uri`, `signal_keys_are_per_record_and_global_keeps_the_bare_uri`, `recall_signals_are_per_record_keyword`, `recall_signals_are_per_record_hybrid`.
- E5: `scoped_link_between_two_global_records_is_refused`, `scoped_link_from_an_own_record_to_a_global_record_is_allowed`, `scoped_link_from_a_uri_another_scope_also_holds_is_refused`, `free_text_rel_is_refused_on_link_and_accepted_on_unlink`, `valid_rel_is_one_lowercase_token`, `wholly_in_scope_needs_every_live_row_in_the_scope`, `edge_guard_hides_invisible_endpoints_and_refuses_write_denied_identities`, `edge_ownership_over_http`.
- E6: `explicit_write_scope_policy_defaults_off_and_toggles_per_tenant`, `opens_a_0_3_6_iam_db_and_adds_the_policy_column`, `scope_binding_applies_the_explicit_write_scope_policy_to_unbound_tokens_only`, `missing_explicit_write_scope_refuses_every_write_and_no_read`, `explicit_write_scope_policy_over_http`, `remote_client_write_scope_satisfies_the_policy_end_to_end`, `write_scope_must_travel_as_a_header_or_is_refused`, `scope_flag_is_global_and_keeps_an_empty_value`, `only_the_flag_names_a_global_write_an_empty_env_is_unset`, `embedded_write_scope_follows_the_flag_unless_dm_scope_binds_the_session`, `an_unusable_write_scope_header_never_reads_as_absent`, `an_unusable_write_scope_header_is_refused_over_http`, `valid_write_scope_is_printable_ascii_without_commas`, `capabilities_route_reports_write_scope_support_per_token`, `client_refuses_scoped_writes_to_a_server_older_than_0_3_7`, `client_reports_a_save_that_did_not_land_in_the_requested_scope`, `client_refuses_scope_for_a_scope_bound_token`.
- E7: `restricted_import_at_is_stamped_and_keeps_the_migration_levers_for_the_operator`, `import_at_refuses_write_denied_identities_and_foreign_agent_trees`, `full_tenant_import_at_is_unchanged`, `import_migration_form_goes_through_the_guards_over_http`, `import_migration_form_needs_the_explicit_write_scope_on_a_policy_tenant`.
- Review follow-up: `read_restricted_global_writer_is_bounded_to_global_rows`, `read_restricted_global_writers_are_bounded_over_http`, `a_capped_adapter_writes_only_inside_its_cap`, `a_capped_adapter_stays_inside_its_cap_over_http`, `batch_graph_maintenance_is_full_tenant_only`, `reindex_is_full_tenant_only_over_mcp_and_rest`, `write_denied_and_guard_refusals_are_typed`.
- Migration: `opens_a_pre_slot_store_and_migrates_its_vectors`, `pending_vector_indexes_migrate_at_start_up_and_block_only_their_own_tenant`, `an_interrupted_slot_migration_resumes_after_its_checkpoint`.
