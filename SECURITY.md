# Security policy

## Supported versions

Security fixes target the latest release on
[GitHub Releases](https://github.com/wakbijok/dm-lite/releases). Older tags
may not receive backports.

## What to report

Please report in private if you find:

- Auth bypass or token leakage around `dmem serve` / IAM
- Path traversal or arbitrary file read/write via the CLI or server
- Remote code execution or unsafe deserialization
- TLS / certificate handling flaws in the built-in server
- Anything that lets one tenant read or write another tenant's data

Out of scope for private security reports (use a normal issue instead):

- Feature requests and UX nits
- DoS via "I sent a huge payload" without a clear bug
- Issues that only appear with custom unsafe builds or stripped sandboxing

## How to report

Prefer **GitHub Security Advisories** (private) for this repository:

1. Open https://github.com/wakbijok/dm-lite/security/advisories/new
2. Include version/tag, OS, feature set (`dist` vs custom), and a minimal PoC

If advisories are unavailable, email: **arifchehusin@gmail.com** with subject
`dm-lite security`. Do not attach exploit chains to public issues.

## Response

We will acknowledge when we can, triage severity, and coordinate a fix and
disclosure. Please give a reasonable window before public write-ups.

## Safe harbor

Good-faith research that follows this policy and avoids privacy harm, data
destruction, and service disruption is welcome. Do not access other users'
data or production systems you do not own.
