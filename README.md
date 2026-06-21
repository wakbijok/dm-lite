# dm-lite

daimon-memory v2: a small typed memory engine for AI agents. One binary, client/server: run `dmem serve` locally (a managed loopback daemon) or on a host, and point the CLI and agent hooks at it (local versus remote is just the URL). Hybrid recall (keyword and dense vector), bitemporal history, and multitenant storage (one database per tenant).

## Features

- One memory across your AI tools: the same recall and capture in Claude Code, Codex, Hermes, and Devin, with the integration built in (one command per tool), not left for you to wire yourself. Switch tools, keep the same brain. Any other MCP client can connect too.
- Typed, curated memory: decisions, lessons, incidents, runbooks, conventions, reminders, and more, each a first-class kind.
- Hybrid recall: SQLite FTS5 keyword search fused with dense vectors (bge-small via zvec), ranked together.
- Bitemporal: every save is a new version, nothing is overwritten; recall the store as of any point in time.
- Client/server in one binary: run it locally, or host it on a VPS, homelab, or cloud box and point your machines at it. Local or remote is just a URL.
- Multitenant: one database per tenant, token-only IAM (root admin plus per-tenant tokens), built-in TLS (no reverse proxy).
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
dmem bootstrap --claude     # or --codex / --hermes / --devin / --all
```

Out of the box this runs on one machine: the server and your client live together. To run the server on one host and connect clients from elsewhere, see the wiki.

## Docs

Full documentation is in the [project wiki](https://git.wakbijok.uk/daimon/dm-lite/-/wikis/home): install and first run, wiring each agent, run as a server, run as a client, multitenant admin, persona and governance, migrating from v1, upgrading, and building from source.

License: MIT.
