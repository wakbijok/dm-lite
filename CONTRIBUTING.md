# Contributing to dm-lite

Thanks for taking an interest. dm-lite is a small Rust binary (`dmem`): typed
memory for AI agents, hybrid recall, client/server.

## Before you start

1. Read the [README](README.md) and the
   [wiki](https://github.com/wakbijok/dm-lite/wiki).
2. Search existing
   [issues](https://github.com/wakbijok/dm-lite/issues) and
   [PRs](https://github.com/wakbijok/dm-lite/pulls) first.
3. For a non-trivial change, open an issue first so we can agree scope.

## Development setup

```bash
# release-shaped build (what the GitHub release workflow ships)
cargo build --release --features dist

# tests (add features your change touches)
cargo test --features dist
```

Useful feature flags are documented in `Cargo.toml` (`dist`, `candle`,
`zvec`, `server`, `client`, `ui`, `model2vec`, …). Default features are empty
on purpose: bare `cargo build` is a lean core, not the friend-facing binary.

## What makes a good contribution

- Small, focused diffs. One concern per PR.
- Tests for behaviour you change (engine, server, hooks, CLI).
- Docs when user-visible behaviour changes (README and/or wiki).
- No secrets, no private hostnames, no personal data in the tree.
- No AI attribution trailers in commits (`Co-Authored-By: Claude`,
  `Generated with …`, session links). Plain technical commit messages.
- Prefer ASCII hyphen `-` over em/en dashes in commit subjects and public docs.

## Pull requests

1. Fork and branch from `main`.
2. Keep the PR description concrete: problem, approach, how you tested.
3. Use the PR template checklist.
4. Expect review on correctness, footprint (RAM/CPU for the embedder path),
   and dual-mode behaviour (local serve vs remote client).

## Issues

Use an issue template when one fits (bug, feature). Include:

- `dmem --version` / release tag
- OS and install path (release binary vs self-built features)
- Whether you run embedded, local `dmem serve`, or remote client
- Minimal steps to reproduce; logs from `dmem doctor` when relevant

## Security

Do not file public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

## License

By contributing, you agree your contributions are licensed under the MIT
License ([LICENSE](LICENSE)).
