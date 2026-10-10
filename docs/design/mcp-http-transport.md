# MCP streamable-HTTP transport on `dmem serve`

Status: implemented in 0.3.7 (`POST /mcp`, `server` feature). Design opened 03-09-2026 on branch `feat/mcp-http-transport`.

## Problem

`dmem mcp` is a stdio adapter: an MCP-aware agent spawns it as a subprocess and it forwards tool calls to the server API. That works for agents on a host where the binary is installed. It does not work for agents that run in-cluster or on a remote host and expect to reach an MCP server over HTTP — those need a sidecar gateway wrapping the stdio process, which duplicates auth and adds a moving part.

The server already has everything an HTTP transport needs: axum, bearer → tenant auth, TLS. The MCP layer is already transport-agnostic (`mcp::handle` maps one JSON-RPC value to one response value). What is missing is the route.

## Proposal

Add `POST /mcp` to the `server` feature, implementing the MCP streamable-HTTP transport (protocol `2025-03-26`, already in `SUPPORTED_PROTOCOLS`).

- Auth: same bearer → tenant resolution as every other route. A missing or revoked token is 401 before any JSON-RPC parsing.
- Request body: one JSON-RPC request or notification. Dispatch through `mcp::handle` with the tenant's `Memory`.
- Response: `application/json` with the JSON-RPC result. Notifications return 202 with an empty body. No server-sent events in the first cut — nothing we expose streams, and clients must accept plain JSON per the spec.
- `GET /mcp`: 405. We have no server-initiated messages.
- Stateless: no `Mcp-Session-Id`. Every call carries the bearer, which is the session.
- Tool surface is identical to stdio (`tool_schemas`, `prompt_schemas`). No transport-specific tools.

## Write policy

Agents reaching memory over HTTP get their own tenant token, never a shared one. Anything an autonomous agent writes (`log_incident`, `remember`) is then attributable to that tenant and reviewable. Read-only agents should get a token whose tenant has no write grants once IAM supports scoped grants; until then, operational policy.

## Non-goals

- SSE (legacy) transport.
- OAuth / dynamic client registration.
- Resumable streams.

## Tests

- Route test: bearer missing → 401; valid bearer + `tools/list` → 200 with the same schema list as stdio.
- Notification → 202, empty body.
- Existing `mcp::handle` tests stay as-is; the route must not fork the dispatch logic.
