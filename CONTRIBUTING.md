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

## Sign your commits (DCO)

Every commit must carry a `Signed-off-by:` line. It certifies the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):
in short, that you wrote the change or otherwise have the right to submit it
under this project's licence, and that you understand the contribution and
your sign-off are public and kept on record.

```bash
git commit -s -m "fix(recall): ..."
```

`-s` appends `Signed-off-by: Your Name <you@example.com>` from your git
config. Use your real name and an address you can be reached at. To sign off
commits you already made on a branch:

```bash
git rebase --signoff main
```

A PR with unsigned commits will not be merged until they are signed off.

## Pull requests

1. Fork and branch from `main`.
2. Keep the PR description concrete: problem, approach, how you tested.
3. Use the PR template checklist.
4. Sign off every commit (see above).
5. Expect review on correctness, footprint (RAM/CPU for the embedder path),
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

dm-lite is licensed under the Apache License, Version 2.0
([LICENSE](LICENSE), attribution in [NOTICE](NOTICE)).

By contributing, you agree that your contributions are licensed under the
Apache License, Version 2.0, as described in section 5 of the licence
("Submission of Contributions"), with no additional terms or conditions. You
keep the copyright in your contributions. If your change includes code you
did not write, say so in the PR and name its source and licence; it must be
compatible with Apache-2.0.
