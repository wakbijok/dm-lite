# mTLS client certificate for the remote client (0.5.2)

Status: built on branch `feat/mtls-client-cert` (from main `22f6ba5`, 0.5.3); was written for 0.5.2 and ships in the release after 0.5.3. Design agreed with Laptop Work on 11-11-2026 (answers below). Requested by Laptop Work, approved by Arif.

## Problem

The remote client proves who it is only with the bearer token. The public endpoint (`https://dmem.wakbijok.uk`) is reachable from outside the LAN, so a leaked token alone is enough to use the memory. A client certificate adds a second factor that lives in a file the token never travels with. The client today cannot present one: `RemoteClient::new` (`src/client.rs`) adds only `ca_cert` to the reqwest builder.

## Scope

In 0.5.2: the client presents a certificate and key on every remote call, `dmem doctor` reports on it, and the files are checked before use.

Not in 0.5.2: enforcement in `dmem serve`, which keeps `with_no_client_auth()`. Caddy enforces on the public endpoint `dmem.wakbijok.uk` (decision D1). `dmem serve --tls-client-ca` is parked as a separate idea.

## Configuration

```toml
[server]
url = "https://dmem.wakbijok.uk"
token = "..."
client_cert = "/path/to/client.pem"   # certificate, PEM; may include the issuing chain
client_key  = "/path/to/client.key"   # private key, PEM: PKCS#8, SEC1 (EC) or PKCS#1 (RSA)
```

- `DM_CLIENT_CERT` and `DM_CLIENT_KEY` override the config, field by field, the same way `DM_TOKEN` overrides `token`.
- Both or neither. A cert without a key, or a key without a cert, is an error naming the missing one; it is never a silent fall back to token-only.
- `dmem login <url> --client-cert <pem> --client-key <pem>` and `dmem setup` write the two fields. They store the paths, never the file contents.
- `ca_cert` is unchanged: it pins the server's CA when set; otherwise the webpki roots are used, as today. The client certificate plays no part in verifying the server.
- Encrypted (passphrase) keys are refused with a clear message. reqwest/rustls cannot decrypt them, and a passphrase prompt would hang the hooks.

## Where it is used

- `RemoteClient::new` reads both files once and adds `reqwest::Identity::from_pem(cert + key)`. Every remote path goes through it: CLI, MCP tools, hooks and `dmem status`.
- `dmem upgrade` keeps its own client (`upgrade::http`), which never reads `[server]`, so the identity is never offered to GitHub. A test asserts that `upgrade::http` has no identity even when `DM_CLIENT_CERT`/`DM_CLIENT_KEY` are set.
- `dmem migrate --url` (v1 export) is unchanged: it talks to an old v1 server, not to `[server]`.

## File checks (before the key is read)

On Unix, for the key file:

- Refuse a symlink (checked with `lstat`, so a link to the real key is refused too; point the config at the file itself).
- Refuse a key readable or writable by group or others (mode & 0o077 != 0), with the `chmod 600` fix in the message.
- Refuse a key not owned by the current user.

The certificate file is public, so it needs only to be a regular file. On Windows the mode and owner checks are skipped and the note in `doctor` says so; the symlink check stays.

The key bytes are held only while the Identity is built and are zeroised afterwards. They are never logged, printed, or included in an error message; errors name the path only.

## doctor

New lines in remote-client mode, text and `--json`:

```
client cert : CN=bukumek, issuer CN=dmem client CA, expires 2027-11-11 (365 days)
```

- Subject, issuer, serial and notAfter, read from the certificate. Never anything from the key.
- A warning when notAfter is within 30 days, an error when it has passed or notBefore is in the future.
- An error when the key does not match the certificate, or a file check above fails.
- `none (token only)` when no client cert is configured.

Parsing the certificate needs an X.509 parser: `x509-parser` under the `client` feature (rcgen already uses it optionally, so it is in the ecosystem we audit). It goes through `cargo audit` and the licence list like every new dependency.

## Errors

When the server or Caddy rejects the handshake for a missing or unknown certificate (TLS alert `certificate_required`, `unknown_ca` or `bad_certificate`), the request error gets a hint: "the server wants a client certificate: set [server].client_cert and client_key (see dmem doctor)". This works the way `TLS_TRUST_HINT` does today.

## Tests

- A local rustls server that requires a client certificate from a test CA (rcgen, throwaway keys):
  - with the right cert, a request succeeds;
  - with no cert, it fails with the hint;
  - with a cert from another CA, it fails.
- Config: the env overrides, and the both-or-neither rule.
- File checks: a symlinked key, a 0640 key and a key with a mismatched cert are each refused, and the message names the path but contains no key bytes.
- `upgrade::http` carries no identity with the env vars set.
- doctor: subject and expiry shown, a 30-day warning at 29 days, an error for an expired cert, and no key material in the output (grep the output for the key's base64).

## Rollout

1. Build on this branch. Run the gate in a Lead izudev slot. Security Review.
2. Soak on izudev against dmem.vps, token plus cert, with enforcement off. This checks that sending a cert changes nothing where it is not asked for.
3. Laptop Work turns on `client_auth` in Caddy on `dmem.wakbijok.uk` once every client has its cert. `dmem serve` listens only on the WireGuard address 192.168.217.1:8077, so that path stays token-only. Final version and timing are Arif's.

## Decisions (Laptop Work, 11-11-2026, within what Arif approved)

- D1. Enforcement is at Caddy on `dmem.wakbijok.uk` only. `dmem serve` listens only on the WireGuard address 192.168.217.1:8077, and each WireGuard peer already has its own key, so that path stays token-only. There is no `--tls-client-ca` in 0.5.2.
- D2. Certs come from a small private client CA on bukumek. Its key is in a 0700 directory, never on the VPS or in a repo. The CA is valid 5 years.
  - Each device leaf is valid 1 year, has EKU clientAuth only, and has CN = the machine name.
  - Caddy trusts the CA and also pins each leaf (`trusted_leaf_certs`), so revoking one is a one-line removal.
  - Keys are generated on the device where possible; only the CSR goes to the CA.
  - With 1-year leaves, the 30-day doctor warning stands.
- D3. Two PEM files only: no PKCS#12 and no Keychain. The hooks must be non-interactive.
- D4. One cert per machine (bukumek, izuhomeland, izudev, mygovuc-bastion, and any new device), so each can be revoked alone.

Build note from D2: doctor checks that the certificate's EKU includes clientAuth, and says so when it does not. Caddy would refuse such a cert at the handshake with a less clear message.

## As built (13-11-2026)

- `src/clientcert.rs`: `pair`, `load`, `inspect`, `validity`, `report`. The key is opened with
  O_NOFOLLOW and every check (regular file, mode, owner) is made on the open file, so it is the
  file that is read; the bytes are in a `Zeroizing` buffer, capped at 64 KiB.
- The key/certificate match is checked with rustls (`CertifiedKey::keys_match`) at load time, so a
  wrong key fails with its path, not as an opaque handshake error. Algorithms: whatever the
  aws-lc-rs provider loads from PKCS#8, SEC1 or PKCS#1 PEM.
- Not done in this build: nothing of `dmem serve --tls-client-ca` (parked, D1) and no Windows
  owner/mode checks (documented in `doctor`'s text for that platform when it is built there).
- Tests: a local rustls server that requires a client certificate from a throwaway CA (accepts the
  right one; the missing and the foreign-CA cases fail, the first with the hint); a server that
  does not ask works with a certificate configured; `http_builder()`/`upgrade::http` send none;
  file checks (symlink, 0640 mode, mismatched key, passphrase, junk); doctor at 29/31 days, expired,
  not yet valid, serverAuth-only, unset; config precedence.
