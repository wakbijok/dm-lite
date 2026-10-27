# Changelog

Notable changes to dm-lite (`dmem`). Releases up to and including 0.3.6 are
described on [GitHub Releases](https://github.com/wakbijok/dm-lite/releases).

## 0.4.3 - 2026-10-27

A build fix for 0.4.2 on macOS and Windows. Linux behaviour is unchanged.

### Fixed

- **macOS and Windows builds.** The 0.4.2 vector-index probe used Linux- and Unix-only calls
  without a platform guard, so `dmem` did not compile for `aarch64-apple-darwin` or
  `x86_64-pc-windows-msvc` and 0.4.2 has no release for them. `close_range` is now used on
  Linux only, with the `fcntl` loop on other Unix systems. On Windows the probe skips that step
  (a child there inherits only handles marked inheritable) and reports a failed open by exit
  code, since Windows has no signals.

## 0.4.2 - 2026-10-27

A follow-up to 0.4.1: damage to a vector index that the size check cannot see no longer
leaves a tenant keyword-only, and the extra checks after an unclean stop cannot hold up
readiness for long.

### Fixed

- **An index the last run did not close is opened in a child process first.** Opening a
  vector index writes `<tenant>.dirty` beside it; a clean close (flushed and synced) removes it.
  When the marker is there at open, the last run was killed or lost power, so `dmem` opens the
  index in a child process (`dmem __probe_vector_index`) before opening it itself. A crash
  there (a native one in zvec), a failed open, or no answer within
  `DM_VECTOR_PROBE_TIMEOUT_SECS` (default 20) costs only the child. The index is then moved
  aside and rebuilt from the database, as a torn one is.
  - `dmem serve` probes every dirty index before it opens its tenants, up to
    `DM_VECTOR_PROBE_CONCURRENCY` (default 4) at a time. After a power-off, when every index is
    dirty, readiness waits for about ceil(dirty / 4) x the timeout in the worst case (10 tenants:
    at most a minute) and usually about a second. A clean start probes nothing and waits for
    nothing.
  - This covers damage the size check cannot see. A manifest that is damaged but not empty
    made zvec fail the open, and the tenant stayed keyword-only until someone rebuilt it by hand.
  - Indexes closed cleanly are not probed, so the cost falls only after an unclean stop.
  - An index another process has open is in use, not damaged: the probe child reports the
    held lock (exit 75) and nothing is moved. A server keeps its index's dirty marker while
    it runs, so without this a CLI command beside it would have quarantined the live index.
  - `DM_VECTOR_PROBE=0` turns it off.
  - A program that embeds dm-lite (not `dmem`) skips the probe unless `DM_VECTOR_PROBE_EXE`
    names a `dmem` to run it with.
- **A background vector rebuild finishes its record before the server stops.** A rebuild
  rewrites one record's vectors at a time (delete, then insert). A stop now waits, within its
  deadline, for a running rebuild to reach the next record before it flushes the index, so no
  record is saved with no vector. A heal that brings back a fresh index runs the full rebuild
  on the retry thread instead of a second, narrower one alongside it.
- **The probe child inherits no index lock.** zvec opens its files without close-on-exec, so
  any child would hold the descriptors of every open index. The probe marks them all
  close-on-exec before it runs.
- **Syncing an index after a flush skips its `LOCK` files**, which hold no data.

## 0.4.1 - 2026-10-27

A fix release after the 27-10-2026 power-off, which left a tenant's vector index torn and
put a dmem server into a crash loop.

### Fixed

- **A torn vector index no longer crashes dmem on every start.** On 27-10-2026 a hypervisor
  power-off cut a vector flush off mid-write and left a tenant's newest zvec manifest at 0
  bytes. zvec's native open does not fail on that: `dmem serve` died with SIGSEGV while
  opening tenants, on every restart, and took the service down. Opening a vector index now
  checks it first. An index with no `manifest.N`, or whose newest one is empty, is moved
  aside to `<tenant>.corrupt-<UTC time>` (a rename; nothing is deleted), and a fresh index
  takes its place, rebuilt from the database. The check sees file names and sizes only: a
  manifest that is torn but not empty still reaches zvec.
- **A fresh vector index next to existing records is filled.** Before, an index created while
  the tenant's database already had records stayed empty for good (on 27-10-2026 one was moved
  aside by hand), so every record dropped out of hybrid recall, which gates on cosine. Now
  such an index gets a `<tenant>.backfill` marker and is rebuilt from the database:
  - `dmem serve` rebuilds on a background thread, one record per save permit. The tenant
    serves meanwhile (records not yet rebuilt are found by keyword) and readiness is not held
    up. A stop part-way repeats the rebuild on the next start.
  - The CLI and the hooks rebuild during the open, resumably, with a checkpoint.
- **The vector index is synced after a flush.** zvec writes its files without syncing them.
  After dmem's own flush (at a planned stop) every file and directory of the index is now
  synced, so a power-off right after a stop cannot leave an empty manifest. zvec's own writes
  between flushes are not covered: the open-time check above guards against those.

## 0.4.0 - 2026-10-20

The last feature release of dm-lite: the held branches are merged
(the managed-install upgrade fence and release packaging, the WorkBuddy and
CodeBuddy bootstrap target, and whole-record recall). From here dm-lite takes
fixes.

### Added

- **`scripts/package-release.sh`**: makes the release archive from a binary that is
  already built. The archive is flat, with exactly `LICENSE`, `NOTICE`, `README.md`,
  `THIRD-PARTY-aws-lc.txt`, `THIRD_PARTY_LICENSES.md`, `dmem` and `libzvec_c_api.so`, and
  it has the same bytes from any checkout path, umask, time zone or input file times
  (sorted members, owner 0, fixed modes, the commit's time). It refuses a dirty tree, a
  version with no changelog section, a binary with a RUNPATH other than `$ORIGIN` or with a
  build path under `/home`, `/Users` or `/root` inside it (the 0.3.8 binary has both), a
  binary that is not the `dist` build or not this version, an AWS-LC text that is not the
  locked `aws-lc-sys`'s licence file, a stale `THIRD_PARTY_LICENSES.md`, and an archive that
  exists. It never builds, signs, tags or uploads; it prints the `rsign` command, whose
  trusted comment is `file:<archive>`, a tab, `version:X.Y.Z` (never a space: `dmem upgrade`
  splits on tabs). `scripts/test-package-release.sh` tests it with stand-in ELF files.
- **`THIRD-PARTY-aws-lc.txt`** is now tracked (the licence file of the locked
  `aws-lc-sys`), and `release.yml` copies it into every archive, as NOTICE says. The
  0.3.8 archive carried it only because it was packaged by hand.
- **`THIRD_PARTY_LICENSES.md`**, the licence text of every Rust crate compiled into the
  `dist` build for the four release targets, is made by cargo-about from `about.toml` and
  `about.md.hbs` (`scripts/package-release.sh --write-notice`), and the script refuses to
  package when the committed file is not what cargo-about generates. It is committed
  and ships in every archive from 0.4.0 on.

- **`docs/releasing.md`** is the release runbook that matches the scripts: the two lanes
  (hand and workflow) and which one is in use, the gate (`cargo test --features dist`), what a
  host without network needs (every locked `.crate` file in the cargo cache, and
  `ZVEC_LIB_DIR` with `ZVEC_AUTO_BUILD=0` for the native library), the build command that
  keeps build paths out of the binary, packaging, signing, verifying, and a written GitLab
  upload step.
- **The release signature carries `file:<archive>`, a tab, `version:X.Y.Z`** as its trusted
  comment when made by `scripts/package-release.sh`'s command or `contrib/sign-release.py`
  (which now also takes `--packaged DIR`, the output directory of the packaging script, in
  place of a workflow artifact). `dmem upgrade` verifies it as before; a test signs a fixture
  with exactly that comment and passes it through the verifier, and shows that the
  space-separated form the hub's own tooling writes would be refused.

- **`dmem bootstrap --workbuddy`** wires every Tencent WorkBuddy and CodeBuddy home
  present (`~/.workbuddy-ai`, which current WorkBuddy builds use, then `~/.workbuddy` and
  `~/.codebuddy`). `settings.json` gets the SessionStart persona and UserPromptSubmit
  recall hooks, and `memory.autoMemoryEnabled` is set to `false` in the same atomic write,
  so the runtime's own `MEMORY.md` auto-memory does not build a second store beside dmem;
  `--remove` restores the stock default. `.mcp.json` gets the `dmem` MCP server. WorkBuddy
  app homes also get a local plugin marketplace with a `dmem@dmem` plugin that declares the
  server, because the app starts its sessions with `strictMcpConfig` and ignores the
  user-scope `.mcp.json`; other marketplaces in `known_marketplaces.json` are kept, and a
  file that is not strict JSON is refused (with a `.dmbak` copy). WorkBuddy homes also get
  the persona and protocols in a marked dmem-managed block in `SOUL.md`, with pointers in
  `IDENTITY.md` and `USER.md`; text outside the marked blocks is kept. The setup wizard lists
  the target.
- **`dmem recall --full`** prints each record's body whole (the CLI printed titles
  only), and the MCP `recall` tool takes `full: true` for the same.

### Changed

- **`dmem upgrade` refuses on a managed install.** A host installer owns the version
  set it installs, so on its hosts `dmem upgrade` stops before it makes any request,
  prints that the install is managed, names `daimon-expert upgrade`, and exits 3
  (other failures still exit 1). An install is managed when the file
  `.daimon-managed` is in the directory of the real binary (the path after symlinks
  are resolved; the installer writes it root-owned), or when `DM_MANAGED` is `1`,
  `true`, `yes` or `on` (any case) in the command's own environment. The marker is
  the fence that holds: a service unit's `Environment=` does not reach an operator's
  shell. The fence fails closed: a program path that cannot be found or resolved, or
  a marker lookup that fails with anything but "not found", is a refusal. `--yes`
  only skips the question after a verified download, so it does not get past it.
  Only a build with `dmem upgrade` (the `self-update` feature, so `dist`) has the
  fence. See docs/managed-install.md.

- **Recall shows whole records.** Every recall surface (the per-prompt hook, the MCP
  `recall` tool and the `recall` prompt) clipped each body at 240 characters, which cut a
  decision before its context and a lesson before its fix. The block now has a budget of
  6000 characters shared between its hits, at least 240 and at most 2000 per record: a few
  hits render whole, and a wide result set is clipped no harder than before.
  `DM_RECALL_BODY_CHARS` sets the per-record limit on every surface (`0` never clips).

### Security

- **rustls 0.23.46, rustls-webpki 0.103.16 and webpki-roots 1.0.10** (compatible
  updates; nothing else in `Cargo.lock` moves). They fix RUSTSEC-2026-0349 (an
  unauthenticated ServerHello could downgrade later connections to a classical key
  exchange, which undoes the post-quantum key exchange of 0.3.8), RUSTSEC-2026-0346,
  -0347 and -0348 (rustls), RUSTSEC-2026-0382 (rustls-webpki could accept a distrusted
  intermediate) and RUSTSEC-2026-0381 for webpki-roots 1.x (distrusted emSign
  intermediates).
- **Accepted for this release**, because fixing them needs major upgrades of candle and
  hf-hub:
  - RUSTSEC-2026-0367, -0368 and -0369 in zip 1.1.4. candle reads model archives with
    `ZipArchive` and never calls `extract`, which is where all three are.
  - RUSTSEC-2026-0381 in webpki-roots 0.26.11, the root store of the HTTPS client that
    hf-hub uses to download the embedding model.

## 0.3.8 - 2026-09-30

Server stability before a replicated deployment: every change below but the
post-quantum TLS key exchange and the operator's import into a room targets
`dmem serve` running as a single writer next to a replication sidecar.

### Fixed

- **No more "database is locked" from concurrent writers.** Every tenant
  database transaction (a save, forget, invalidate, or the access-signal bump
  after a recall) now starts with `BEGIN IMMEDIATE`, set once when the
  connection opens, and so does `iam.db`. Before, a transaction started as a
  reader and upgraded on its first write; when another connection to the same
  file (a second `dmem` process, a replication sidecar) had committed in
  between, the upgrade failed at once with "database is locked", which
  `busy_timeout` cannot retry. A test with four connections writing one file
  saw 43 of 320 writes fail that way; now each waits its turn and none fails.
  Schema migrations already used `BEGIN IMMEDIATE`. A recall with no hits
  bumps no access signal and so opens no transaction: it never waits for the
  write lock.
- **A long save no longer stalls its tenant.** `dmem serve` embedded a save's
  body while holding the tenant's lock, so every other request for that
  tenant waited for the model: a save of a 64-window body blocked a recall on
  the same tenant for 93.6 s (a recall on another tenant took 36 ms). Each
  route that embeds (the typed saves, `/remember`, `/import`, `/recall`,
  `/recall_expanded`, and the MCP save tools, `recall` tool and `recall`
  prompt) now embeds its texts before it takes the lock and hands the vectors
  over; the lock covers only the SQLite write and the vector upsert. The
  vectors are the same ones as before (same embedder, same text). Texts no
  route knows in advance (the re-embed after an invalidation that changes a
  record's current text, and the one-time vector slot migration) still embed
  under the lock. The tenant lock had also been the only bound on embedding
  (one per tenant at a time); a bound of its own replaces it, see
  `DM_EMBED_CONCURRENCY` below.
- **Graceful stop on SIGTERM.** `dmem serve` over TLS had no stop handling
  (SIGTERM's default action killed the process mid-request) and the
  plain-HTTP path only caught ctrl-c, so every rollout, drain or service
  restart was a crash-stop. Both paths now
  stop on SIGTERM or SIGINT: no new connections, in-flight requests finish
  for up to `DM_SHUTDOWN_GRACE_SECS` (default 15 s; 0-3600), then every open
  tenant's vector index is flushed (new `ZvecIndex::flush`) and the tenant
  closed, and the process exits 0. A request that reaches a tenant after the
  close began answers 503, also one that was already waiting for the
  tenant's lock when it began, so nothing is written after the flush. After
  the grace a stop takes up to 7 s more plus the flushes (the TLS drain's
  1 s backup, up to 5 s for a tenant lock a straggler holds, 1 s for the
  runtime to let go). A tenant still in its first open (the start-up
  vector slot migration) is left to resume on the next start, and the
  migration starts no further tenant once a stop arrived. The log shows each
  step (`SIGTERM received ... draining N in-flight request(s)`, `closed N
  tenant(s), vector index flushed for N`, `shutdown complete`). In a test, a
  vector index killed with SIGKILL right after three saves kept all three
  vectors (it writes its own log), so the flush is about leaving a
  consistent on-disk state for snapshots, not about process crashes.
- **The model cache follows `HF_HOME` and `HUGGINGFACE_HUB_CACHE`, and
  `HF_HUB_OFFLINE=1` stays offline.** The model loader read only
  `~/.cache/huggingface/hub`, whatever `HF_HOME` or `HUGGINGFACE_HUB_CACHE`
  said, though the README and `dmem doctor` (which looked there) said they
  were honoured; and it never read `HF_HUB_OFFLINE`. A site that
  pre-populated a cache elsewhere had dmem download the model again, reaching
  the model host even with `HF_HUB_OFFLINE=1`, or, air-gapped, fall back to
  the placeholder. The cache is now `HUGGINGFACE_HUB_CACHE`, else
  `$HF_HOME/hub`, else `~/.cache/huggingface/hub` (unchanged for a site that
  sets neither), for the loader and `dmem doctor` alike, and with
  `HF_HUB_OFFLINE=1` (or `true`, `yes`, `on`) the model loads from that cache
  only: nothing is downloaded. `dmem doctor` counts the model as cached only
  when the snapshot `refs/main` names holds every file the loader reads.
- **`dmem skills sync` and `dmem skills list` load no model.** They read the
  local tenant's store, and opened the whole engine to do it, so a remote
  client loaded (or downloaded) the embedding model just to list skills. They
  now read the store directly.

### Added

- **`GET /readyz`.** A readiness probe beside the static `/healthz`: 200
  when the server should take traffic, 503 naming the failing checks when the
  IAM database is not open (env-only serving) or its file is gone or
  unreadable (`iam`), the embedder is the placeholder (`embedder`), a tenant
  has no vector index (`vectors`), the data dir (or `tenants/`) cannot be
  written (`data_dir`: a new probe file is created, synced and removed), the
  start-up pass is still running (`startup`), or a stop is in progress
  (`shutdown`). It needs no token; without one the body gives each check's
  name and `pass` or `fail`, nothing more, and the admin token or
  `DM_METRICS_TOKEN` also gets each check's detail. No body names a tenant, a
  path or the version; the server log names the tenants, once per change. The
  disk checks are reused for 2 s (one caller refreshes them, concurrent
  probes wait for its result), so polling `/readyz` cannot become an fsync
  loop on the data volume.
- **Readiness judges every tenant before traffic.** At start-up `dmem serve`
  opens every known tenant that has a database (IAM's active tenants and the
  `DM_TOKEN_*` tenants; the start-up vector slot migration first), and
  `/readyz` stays 503 (`startup`) until that pass is done, so a tenant whose
  vector index cannot open fails readiness before the server takes traffic,
  not at its first request. A registered tenant never written to is not
  opened (nothing to judge, and no files are created for it).
- **A vector index that could not open heals without a restart.** A tenant
  without a vector index (keyword-only, or refused under
  `DM_REQUIRE_VECTORS`) is retried in the background, first after 5 s, then
  at doubling intervals up to 5 min, with no request needed (a server that is
  not ready gets none). When the index opens, the tenant's cached handle is
  replaced (requests on the old one finish on it) and readiness returns; then
  every record written while the tenant had no index is re-embedded, outside
  the tenant lock and one record at a time under a save permit, because
  hybrid recall gates on cosine and does not return a record without a
  vector (records saved keyword-only before this process started are not
  covered: `dmem reindex-embeddings`, with the server stopped, re-embeds
  every record). Before, one such fault (a lock another process held on the index,
  a file in the way, a full disk) kept the tenant keyword-only until a
  restart, and a server reporting it through `/readyz` would have stayed out
  of service, `/healthz` still 200, until someone restarted it.
- **`GET /metrics`.** Prometheus text format: tenant lock wait and hold
  times, request latency and responses by route, embedding time before and
  under the tenant lock, the active embedder (and whether it is the
  placeholder), vector index failures by operation (open, migrate, upsert,
  search, flush), open tenants and those without a vector index, requests in
  flight, embedding permits and the time spent waiting for one, readiness and
  the version. It answers the admin token or a scrape-only `DM_METRICS_TOKEN`
  (at least 16 characters, or `dmem serve` refuses to start); tenant tokens
  get 403. Labels carry routes and operations, never tenant names.
- **Bounded embedding: `DM_EMBED_CONCURRENCY` and
  `DM_EMBED_QUERY_CONCURRENCY`.** A forward pass allocates tens of MB and a
  long save runs up to 64 of them, so `dmem serve` now embeds at most
  `DM_EMBED_CONCURRENCY` record bodies at once across all tenants (default:
  half the CPUs, between 1 and 2) and, with permits of their own so a recall
  never queues behind long saves, `DM_EMBED_QUERY_CONCURRENCY` recall queries
  (default 2). Each 1-64. The start-up log states both.
- **Fail closed on the vector index: `DM_REQUIRE_VECTORS`.** A tenant whose
  vector index cannot open silently served keyword-only recall. With
  `DM_REQUIRE_VECTORS=1` its open fails, so its requests answer 500 instead
  of degrading; the open is retried by its next request and on the retry
  schedule above, so the tenant opens once the cause is gone. A build without
  vectors refuses to start. Any value but empty, `0`, `off`, `false` or `no`
  turns it on, so a typo errs on the safe side. It is off by default. (The
  embedding model is required by default: see Changed.)

### Changed

- **The embedding model is required.** Memory is a core item, so dmem no
  longer runs on the placeholder hash embedder (keyword overlap, not
  semantics) when the model cannot load (not cached, no network,
  `HF_HUB_OFFLINE=1` without the cache). 0.3.7 fell back silently and every
  recall degraded. Now `dmem serve` refuses to start (before IAM mints
  anything) and a local tenant refuses to open (the embedded CLI, `dmem
  mcp`, the hooks in embedded mode), with one error that names the model, the
  cache directory it looked in, how to pre-install the model and the escape
  hatch. A build without an embedding model refuses the same way, and so does
  a model that loads but embeds a probe text as anything but a finite,
  non-zero 384-d vector (a `DM_CANDLE_MODEL` checkpoint of another width, a
  tokenizer that does not match its weights): before, it ran with no working
  vectors, reported only by a line on stderr per save or recall, and a save
  whose vector did not fit the index also dropped the record's earlier ones.
  `DM_REQUIRE_MODEL=0` (or `false`, `no`) allows the placeholder, for tests
  and development only: dmem prints one warning line that memory recall is
  degraded, and `/readyz` (still 503; its detail, for the admin or metrics
  token) and `dmem doctor` report `degraded: placeholder embedder`. Any other
  value, or none, requires the model; `DM_REQUIRE_MODEL=1` keeps working. A
  remote client (a `[server]` block) is unaffected: the server embeds, and
  the client never loads or downloads a model. `dmem doctor` adds
  `model_req` and `model_state` (JSON: `model_required`, `hub_offline`,
  `model_state`), predicted from the cache without loading the model.
- **The operator's import keeps its dates in a named scope.** A full-tenant
  token that names a room (`dmem --scope room:ops migrate ...`, an
  `X-DM-Write-Scope` header) may now backdate (`created_ms`) and raise
  `importance` above the kind's default on `/import`, as it already could
  writing global; the record is stamped with exactly the scope it named, and
  its valid time starts at the original creation. 0.3.7 (E7) refused both
  levers to it, so rows could not move into a room with their dates. The
  levers stay refused, as in 0.3.7, to scope-bound tokens (minted with
  `--scope-read`/`--scope-write`) and to adapters, and nothing else widens:
  a full-tenant token writing as a room still retracts and links only that
  room's records and cannot write into the `agents/` tree. It could already
  write the same record global, which every scope reads.
- **`dmem migrate` names what it skips.** Each skipped record gets one line
  on stderr: its line number, its title (the first 60 characters) when it
  has one, and why (the refusal or error, `not JSON`, `no title`). Before, a
  refused record was only counted. The final stdout line is unchanged:
  `migrated N records, skipped M`.

### Security

- **Post-quantum hybrid key exchange on every TLS connection.** `dmem serve`,
  the remote client, `dmem upgrade` and `dmem migrate --url` now offer
  X25519MLKEM768 first: ML-KEM-768 (FIPS 203) combined with X25519, so a
  session's keys stay secret as long as either one holds, and traffic
  recorded today cannot be decrypted later by breaking X25519 alone. X25519,
  P-256 and P-384 follow it for a peer without ML-KEM: a 0.3.8 client reaches
  a 0.3.7 server over X25519 in the same round trip (it sends an X25519 key
  share beside the hybrid one), and a 0.3.7 client or an older browser
  reaches a 0.3.8 server as before. The server follows the client's order, so
  a client that lists the hybrid first, as 0.3.8 does, gets it; 0.3.7 refuses
  a client that offers only the hybrid. Why: Malaysia's direction on
  post-quantum cryptography, with the key exchange hybrid rather than ML-KEM
  alone per CyberSecurity Malaysia's migration framework, so it is never
  weaker than the classical exchange it replaces. rustls' crypto provider
  moves from ring, which has no ML-KEM, to aws-lc-rs, installed once per
  process before any TLS work; aws-lc-rs was already compiled into every
  release build (through axum-server), so building needs nothing new.
  Certificates and their signatures stay classical: the generated certificate
  is still ECDSA P-256, and a certificate you bring is used as it is.
- **The remote client uses rustls in the release binaries too.** A `dist`
  build also links the platform TLS library (hf-hub, which fetches the
  embedding model, brings it in), and reqwest picked it for the remote
  client, `dmem upgrade` and `dmem migrate --url`: OpenSSL on Linux (3.0 on
  Ubuntu 22.04 and 24.04, without ML-KEM), Schannel on Windows, Secure
  Transport on macOS. They now always use rustls with the provider above, as
  a build without the embedding model already did, and trust the Mozilla root
  store built into dmem (webpki-roots) plus a pinned `ca_cert`, not the
  operating system's store. rustls checks a certificate more strictly than
  OpenSSL did: see the upgrade notes for what a pinned certificate needs.
  They speak HTTP/1.1, as before: reqwest in a `dist` build also has HTTP/2
  (hf-hub again), which rustls would offer first and `dmem serve` accepts,
  and it stays off. The model download is unchanged.
- **`dmem serve` creates owner-only files.** The server sets its umask to 077
  before it creates anything, so every file and directory it makes under the
  data dir is owner-only: the tenant databases' `-wal`/`-shm` files, `iam.db`'s
  `-wal`/`-shm`, the vector index and the TLS key. Before, they followed the
  service's umask (0644/0664 under the usual 022/002), while only the database
  files themselves were chmod 0600. The generated TLS certificate is public and
  stays 0644; directories the server creates are 0700, so copy it as the
  service user or root. Files that already exist keep their modes.
- **No bootstrap admin token when `DM_ADMIN_TOKEN` is set.** With
  `DM_ADMIN_TOKEN` in the environment the server never mints, prints or writes
  a bootstrap admin token, now also when the value is empty or not UTF-8 (a
  Secret with a missing key): such a value registers nothing, and start-up says
  so instead of printing a fresh root token into the logs. When
  `DM_ADMIN_TOKEN` is set and an `admin.token` from an earlier start is still
  on disk, start-up names the file (not the token): that token stays valid
  until revoked.
- **Rotating `DM_ADMIN_TOKEN` retires the old value.** A non-empty
  `DM_ADMIN_TOKEN` is the one env admin token: admin tokens registered from an
  earlier value are revoked at start-up (the log gives the count, never a
  token), so a leaked value rotated out of a Secret stops working at the next
  start. Before, every value ever set stayed an admin token. The bootstrap
  token and other admin tokens are untouched, and an empty or unreadable
  value revokes nothing.
- **`/readyz`, the one new route without a token, reveals no detail.** Its
  unauthenticated body is each check's name and `pass`/`fail`: not the
  embedder, not how many tenants are open, and not that IAM is down (which
  would tell anyone that revocation is off); the detail needs the admin or
  metrics token. Its data-dir probe creates a file of its own under a unique
  name, never an existing path and never through a symlink, and runs at most
  once per 2 s however often `/readyz` is called.
- The bootstrap token file is written beside the `iam.db` that minted it (the
  data dir, as before, for `dmem serve`). The test suite used to write it into
  the developer's own data dir.

### Upgrade notes

- Nothing to change for an existing deployment, except on a host whose
  embedding model is not where 0.3.8 looks (next note), and on clients whose
  server certificate rustls refuses: a private CA trusted only through the
  OS store, or a pinned certificate that is itself a CA or names its host
  only in the CN (see the trust store note below). The embedding model is
  now required (`DM_REQUIRE_MODEL` defaults to on); every other new switch is
  off by default (`DM_REQUIRE_VECTORS`, `DM_METRICS_TOKEN`) or keeps a safe
  default (`DM_SHUTDOWN_GRACE_SECS` = 15, `DM_EMBED_CONCURRENCY` = half the
  CPUs within 1-2, `DM_EMBED_QUERY_CONCURRENCY` = 2). No schema change.
- Before upgrading a host that runs `dmem serve` or the embedded engine, run
  the new binary's `dmem doctor` as the service's user with the service's
  environment: its `HOME`, `HF_HOME`, `HUGGINGFACE_HUB_CACHE`,
  `HF_HUB_OFFLINE` and `DM_CANDLE_MODEL`. It must show `model_cached: yes`
  or, on a host that can reach huggingface.co, the service downloads the
  model (about 130 MB) at its first start; with `HF_HUB_OFFLINE=1` or no
  network it must be `yes`, or the service refuses to start. 0.3.8 honours
  `HF_HOME` and `HUGGINGFACE_HUB_CACHE` (0.3.7 ignored them), so a site that
  sets neither loads from the same place as before, and a site that set one
  loads the model from there, not from `~/.cache/huggingface/hub`. Where
  0.3.7 left the model in `~/.cache/huggingface/hub` while one of them was
  set, unset the variable, or copy the model's whole directory into the
  cache doctor names (`model_cache`), keeping its `refs/` and `snapshots/`:
  `mkdir -p "$HF_HOME/hub" && cp -a ~/.cache/huggingface/hub/models--BAAI--bge-small-en-v1.5 "$HF_HOME/hub/"`
  (into `$HUGGINGFACE_HUB_CACHE/` when that is set; a `DM_CANDLE_MODEL`
  override has its own `models--<org>--<name>`). On an offline or
  air-gapped site, pre-install the model into that cache first (README:
  Offline / air-gapped) and set `HF_HUB_OFFLINE=1`. Clients with a
  `[server]` block need nothing. Test and development setups that ran on the
  placeholder set `DM_REQUIRE_MODEL=0`.
- Records saved while dmem ran on the placeholder keep placeholder vectors:
  on a 0.3.7 host whose model did not load (its log says
  `using placeholder embedder`), or in a data dir used with
  `DM_REQUIRE_MODEL=0`. 0.3.8 on the model scores its queries against those
  vectors, so those records recall poorly, with no warning, and importing the
  files again does not help (`dmem import` skips unchanged files). Stop the
  server and run `dmem reindex-embeddings` as the service's user with its
  environment (not `DM_REQUIRE_MODEL=0`, so it refuses unless the model
  loads), once per tenant (`DM_TENANT=<tenant>`), then start it.
- A stop signal now exits 0 after a drain instead of killing the process, so a
  service manager sees a clean stop. Give it a stop timeout of at least the
  grace plus 10 s (systemd's default 90 s is enough). In Kubernetes,
  `terminationGracePeriodSeconds` must be at least the preStop sleep plus
  `DM_SHUTDOWN_GRACE_SECS` plus 10 s, because the preStop counts against it:
  the default 30 s fits a 5 s preStop with the default 15 s grace.
- For probes, use `/readyz` for readiness and keep `/healthz` for liveness. A
  server with a tenant that has no vector index, or on the placeholder
  embedder under `DM_REQUIRE_MODEL=0`, is now reported not ready (503) though
  it still serves. A tenant's vector index heals by itself once the cause is
  gone (see above). A server without its model does not start at all (see
  Changed), so an orchestrator restarts it instead of it sitting out of
  service with a passing liveness probe; `DM_REQUIRE_VECTORS=1` is safe there
  too now that refused tenants are retried.
- Readiness now also waits for the start-up pass that opens every known
  tenant with a database: allow for it in a startup or readiness probe's
  delay when a tenant still needs the one-time vector slot migration.
- If `DM_ADMIN_TOKEN` was ever changed, only its current value is an admin
  token after the first start of 0.3.8: point admin clients at it.
- Saves to different tenants used to embed in parallel without a bound; they
  now share `DM_EMBED_CONCURRENCY` permits. Raise it on a large host that
  serves many tenants.
- Files the server creates from now on are owner-only; existing files keep
  their modes (`chmod -R go-rwx <data dir>` to align them). A replication or
  backup sidecar must run as the same user, as it already had to for the
  0600 database files.
- TLS key exchange needs nothing: 0.3.7 and 0.3.8 clients and servers connect
  in any mix, and the hybrid is used once both ends run 0.3.8. Upgrade
  either side first.
- The remote client, `dmem upgrade` and `dmem migrate --url` in the release
  binaries no longer read the operating system's trust store, and they check
  certificates with rustls instead of OpenSSL, which is stricter (see
  Security). A client that trusted its server's private CA only through the
  OS store now fails with "the server certificate is not trusted": pin the
  CA with `dmem login <url> --ca-cert <ca.pem>` (`dmem migrate --url` takes
  `--ca-cert` too). A pinned certificate that 0.3.7 accepted can be refused
  as well. The server certificate must not itself be a CA: a plain `openssl
  req -x509` makes a self-signed one with CA:TRUE, while the one `dmem serve
  --tls-generate` writes is fine. It must also name the host or IP of the
  URL in a subjectAltName, because the CN alone is no longer read. A private
  CA works when the CA is pinned and the server certificate names the URL's
  host or IP that way. Check the server certificate with `openssl x509 -in
  <cert.pem> -noout -ext basicConstraints,subjectAltName` before upgrading
  its clients: `dmem login` does not test the connection, and a refused
  certificate leaves the hooks injecting no memory, with the error on stderr
  at most. A server with a public-CA certificate needs nothing. `dmem
  upgrade` behind a TLS-inspecting proxy no longer trusts the proxy's CA.
- A full-tenant token that names a room (`X-DM-Write-Scope`, `dmem --scope`)
  is no longer refused `created_ms` or an `importance` above the kind's
  default on `/import` (see Changed). A relay that writes for less-trusted
  callers with a full-tenant token and a forced `X-DM-Write-Scope`, and
  relied on that refusal to stop them backdating records or ranking them
  first in that room's boot layer and recall, must now strip both fields
  itself or move to an adapter token, which is still refused both. A relay
  on adapter tokens needs nothing.
- Library users (`dm_lite`): `LocalMemory::open_tenant` keeps its signature,
  and like every open it now fails without the embedding model unless
  `DM_REQUIRE_MODEL=0`; new are
  `LocalMemory::open_tenant_with_embedder`, `EmbedPlan` and `PreEmbedded` (the
  pre-lock embedding hand-off), `check_model_requirement` and
  `enforce_model_requirement`, `embedder::hf_hub_cache_dir` and
  `embedder::hf_hub_offline`, `flush_vectors`,
  `LocalMemory::vector_index`, `LocalMemory::set_scope_unbound` (the
  migration levers in a named scope, off unless set),
  `tools::resync_uri_vectors`, `SqliteStore::uris_changed_since`,
  `ZvecIndex::flush`, the `config::embed_concurrency` pair and the `metrics`
  module.
  `server::Authenticator` gained `tenants()`, with a default, so existing
  implementations still compile.

## 0.3.7 - 2026-09-29

### Fixed

- **Scope-exact forget and invalidate (E1).** A write-restricted identity (a
  scoped token, or an adapter writing as `X-DM-Write-Scope`) now retracts or
  invalidates ONLY the live rows of a uri in its own write scope; another
  scope's record under the same uri stays live, and the returned count covers
  the caller's rows only. The guard asks for at least one live row of the uri
  in the caller's scope, so the outcome no longer depends on which scope saved
  last (before, a retraction was refused when an invisible scope saved last and
  closed every scope's rows when the caller saved last). A uri's edges and its
  vector go only with its last live row in any scope; otherwise the vector is
  re-embedded from the newest remaining row (only when that row's text differs:
  a plain invalidation keeps the text, so it costs no embedding). Full-tenant
  tokens are unchanged.
- **Dead records stay as hidden as they were (E2).** A uri with no live row is
  now judged by its newest row ever, so a retracted room record's uri (its
  title, in slug form) is no longer shown to other rooms through a dangling
  edge, and a dead `agents/<other>/...` uri stays hidden from other agents. A
  uri that never existed stays visible, as before. An invalidation that leaves
  a uri with no live row in any scope (a cut before the record's start) now
  deletes its edges and vector, as a forget does.
- **Exact limits for scoped readers (E3).** `history`, `recent`, recall's
  keyword channel, as-of recall, `by_kind` and the persona boot layer now apply
  the scope and agent-tree gates in SQL, so `limit` counts visible rows. Before,
  they over-fetched four times the limit and filtered afterwards, so heavy
  churn in a scope the reader cannot see (for example 800 versions of one uri)
  hid the reader's own and global rows. The persona route's agent-tree match is
  now case-insensitive, like every other agent-tree check (`Agents/Analyst/...`
  is the analyst's tree, not shared pool).
- **A room save no longer hides a global record (E4).** When a room saves a
  record with a global record's kind, namespace and title (the same uri, in
  another scope), other rooms still see the global record: `get` and graph
  visibility pick the newest row among the scopes the reader may see, instead
  of letting a newer invisible row hide an older visible one. The vector index
  now keeps one slot per record, per (uri, scope), so the room's save no longer
  overwrites the global record's vector, and a vector hit is hydrated as the
  record it embeds; slots in scopes the reader cannot see are dropped before
  ranking. Recall gives one hit per uri: when the reader sees several records
  under one uri (a room reader: the global record and its own copy; a
  full-tenant reader: every room's copy), the one in the reader's own write
  scope takes the place, else the newest, so copies of one title never fill
  the limit. A reader behind a scope or agent gate searches the vector index
  again with a larger k (up to 1024 slots) when slots it cannot see crowd the
  nearest results, so other audiences' near-duplicates no longer push its
  records out of hybrid recall. Access signals (the bounded ranking boost from
  recall frequency) are per record too: a room recalling its own copy of a
  title no longer raises the rank of the global record under that uri. Signals
  stored before 0.3.7 stay with the global record. A scoped record's slot id
  is a truncated SHA-256 of its scope and uri; the global slot keeps its key.
- **Vector index migration.** An existing vector index is re-keyed once: for
  every uri a non-global scope has ever held a row of, the global slot is
  rebuilt from the live global row (or cleared) and each scope with a live row
  gets its own slot. Records only ever saved globally are untouched. The work
  is one embedding per scoped record (about half a second each with the
  default embedder on a small server). `dmem serve` runs it at start-up for
  every tenant that needs it, one tenant after another on a background thread,
  while it already listens: a request for a tenant under migration waits for
  it, and other tenants and `/healthz` are served meanwhile, so a health
  monitor does not restart the service mid-way. The embedded CLI migrates on
  its first open. Progress is logged and checkpointed per uri in
  `<tenant>.scope-slots.progress`, so an interrupted run resumes where it
  stopped. A `<tenant>.scope-slots` marker next to the tenant's vector
  directory records completion; delete it to run the migration again.
- **Edge ownership in the engine (E5).** A write-restricted identity may add or
  remove an edge only when one endpoint is wholly its own: every live row of
  that uri is in its write scope. It can no longer wire two global records
  together or cut the operator's edge between them, and a uri that another
  scope (or global) also holds does not count as its own end. Both endpoints
  must still be visible to it, and an invisible one still reads as not found.
  Its `link` also needs a token rel (`^[a-z][a-z0-9_-]{0,31}$`), because an
  edge label is readable by everyone who sees both endpoints; `unlink` accepts
  any rel, so legacy free-text edges can still be removed. Full-tenant tokens
  are unchanged, labels included. These refusals answer 400 with the rule that
  refused (for example `invalid rel: ...`) instead of a generic body. Known
  limitation: the ownership test counts every live row of the uri, including
  rows in scopes the caller cannot see, so a refused link tells the caller that
  some other scope holds a record with that exact kind, namespace and title
  (an existence oracle for a guessed title, not its content). The rule is also
  checked at link time only: if another scope later saves the same uri, the
  edge becomes visible to that scope. Edges that carry their creator's scope
  (planned) remove both.
- **Guard refusals answer 400 with their reason.** A `/forget` refused by the
  retraction guard (for example a record not in the caller's write scope)
  answered 500 "internal error". Engine guard refusals (a read-only token's
  write, a record outside the caller's write scope, the `agents/` tree guard,
  the edge rules, the write-scope policy) now answer 400 with the rule's
  message on every route, and MCP tool errors carry the same message. Other
  errors keep their generic body.
- **Imports with `created_ms` or `importance` go through the guards (E7).**
  The migration form of `/import` (`import_record_at`) ignored the caller's
  write scope and the `agents/` tree guard and saved GLOBAL whatever the
  token, so an adapter writing as a room could plant a record every room then
  read, a high-importance protocol included. It now stamps the identity's
  write scope, runs the `agents/` tree guard and refuses a write-denied
  identity, like every other save. A write-restricted identity may no longer
  backdate (400 `created_ms is set by the operator`) or raise `importance`
  above the kind's default (400 `importance is set by the operator`); a lower
  importance is kept. Full-tenant tokens are unchanged; one that names a scope
  with `X-DM-Write-Scope` writes as that scope's writer, and on a tenant with
  the explicit-write-scope policy the migration runs as `dmem --scope ''
  migrate ...`.
- **A read-restricted global writer is not a full-tenant writer.** An adapter
  that promotes a write with `X-DM-Write-Scope: ''` while asserting a
  restricted `X-DM-Scopes`, or a scoped token minted with a `''` write scope,
  writes global but reads only its scopes. The engine took any `''` writer for
  a full-tenant one, so such an identity could retract or invalidate records
  it cannot read (every scope's rows, with a count oracle), link and unlink
  edges between them, plant backdated or high-importance records, and write
  into an agent's `agents/` persona tree. Its retractions now touch only
  global rows, its edges follow the E5 rule with the global scope as its own,
  and the `agents/` tree and the E7 levers are refused. Only an identity that
  writes global and reads the whole tenant keeps those levers.
- **An adapter's scope cap bounds its writes.** An adapter token minted with a
  scope cap could name any write scope, which then also joined its read set,
  so it could read and write a scope outside the cap just by naming it. A
  write scope outside the cap, or `''` (promotion is an uncapped adapter's
  job), now leaves the request read-only, and its writes answer 400 with the
  reason.
- **An unusable `X-DM-Write-Scope` is refused, not ignored.** A header the
  server could not read as text (for example `room:café`) counted as absent,
  so a full-tenant token's write landed in global. A value outside printable
  ASCII, one with a comma, or a repeated header now answers 400 on every route
  and token type. The client refuses such a `--scope` before sending.
- **Batch graph maintenance is full-tenant only on every surface.** The MCP
  `reindex_links` tool ran for any token, so an adapter, a read-only token or
  a full-tenant token that named no scope on a policy tenant could write edges
  (bypassing the E5 rule) and prune edges tenant-wide. `reindex_links` and
  `reindex_mentions` now need a full-tenant identity writing global in the
  engine, over REST and MCP alike.
- **The latest-save time follows the reader's view.** `/latest_save` took the
  newest write in the whole tenant, so a scoped reader could watch when scopes
  it cannot see write. It is now the newest row the reader may see.

### Added

- **Explicit write scope per tenant (E6).** `dmem admin policy <tenant>
  --require-explicit-write-scope true` (admin route `POST /admin/tenant_policy`)
  makes a tenant refuse every write by a full-tenant token that does not name
  its scope: the saves, `/import`, `/forget`, `/invalidate`, `/link` and
  `/unlink`, over REST and MCP, answer 400 `this tenant requires an explicit
  write scope` and store nothing. A write names its scope with
  `X-DM-Write-Scope` (`''` = a deliberate global write). The CLI and the
  remote client send it from the new global `--scope <scope>` flag or
  `DM_WRITE_SCOPE`, and the embedded engine stamps it (a `DM_SCOPE` session
  refuses a different one). Only `--scope ''` names a global write: an empty
  `DM_WRITE_SCOPE` counts as unset, because environment files often export
  variables empty. Reads are unchanged, and so are scoped and adapter tokens
  and tenants without the policy (the default); the batch `reindex_*` routes
  need `--scope ''` on a policy tenant, as they write edges. On every tenant, a
  full-tenant token's `X-DM-Write-Scope` now stamps its writes with that scope
  instead of being ignored. Save replies report the scope they were stamped
  with (a new `scope` field beside `uri`; older clients ignore it), and before
  the first write that carries `--scope` the client asks the server
  (`GET /capabilities`) whether it honours it: against a server older than
  0.3.7, which would ignore the header and store the write in global (or
  retract every scope's rows on a forget), or with a plain scope-bound token,
  which always writes its own scope, the write is refused before it is sent,
  and a save whose reported scope differs from `--scope` is reported as an
  error, never as stored. `dmem admin policy` against an older server says the
  server needs upgrading. The policy is stored in `iam.db` (a
  `tenants.require_explicit_write_scope` column, added on start-up and ignored
  by older dmem) and shown by `dmem admin list`. It is read per request, so a
  change applies at once. With `--allow-env-only` (no IAM) no policy is
  enforced.
- **`GET /capabilities`.** Any tenant token can ask what the server supports:
  its version, whether it understands `X-DM-Write-Scope`, and whether this
  token's writes follow it. Servers before 0.3.7 answer 404.
- **MCP over HTTP.** `dmem serve` answers the MCP streamable-HTTP transport on
  `POST /mcp` (protocol 2025-03-26), stateless, behind the same bearer-to-tenant
  check as every other route (401 before any parsing). It dispatches through the
  same handler as `dmem mcp`, so the tool surface is identical on both
  transports. A notification answers 202 with no body, a malformed body answers
  400 with a JSON-RPC parse error, and `GET /mcp` answers 405. The `initialize`
  instructions are rendered per call from the token's agent view.
- **Library crate.** The module tree is now the `dm_lite` library, and the `dmem`
  binary is a thin CLI over it, so other programs can embed the same engine. No
  behaviour change for `dmem`.
- **Chunk kind and scoped recall.** New record kind `chunk` (a slice of a
  document: namespace = file path, title = section, body = text; default
  importance 40). `LocalMemory::recall_in` restricts recall to namespaces under
  a set of prefixes, and `POST /recall` accepts an optional `namespaces` list for
  the same filter.
- **Claude Cowork wiring (macOS).** `dmem bootstrap --cowork` ensures the
  `mcpServers.dmem` entry in `claude_desktop_config.json`, then installs a
  hooks-only plugin into every Cowork workspace on the machine (SessionStart:
  persona and recent context, skipping `resume`; UserPromptSubmit: recall). It
  also writes `~/.daimon/cowork-plugin/dmem-cowork.plugin` as a manual-upload
  fallback. `--remove` unwires it. The setup wizard offers it as an unchecked
  item.
- **`dmem status`** reports the Claude Desktop and Cowork wiring, and prints the
  agent wiring block in remote-client mode too.

### Changed

- **Deliberate refusals (E6, E7).** These are the only places where a 0.3.3
  or 0.3.6 client gets a different answer from a 0.3.7 server for a request
  it could already send:
  - On a tenant with the explicit-write-scope policy (opt-in per tenant, off
    by default, set with `dmem admin policy`), every write by a full-tenant
    token that does not name its scope answers 400 `this tenant requires an
    explicit write scope` and stores nothing. Older clients cannot name a
    scope, so on such a tenant they can still read but can no longer write.
    Tenants without the policy behave as before.
  - An `/import` with `created_ms` from a write-restricted identity (a scoped
    token, an adapter, or a token that names a non-global write scope)
    answers 400 `created_ms is set by the operator`. One with an `importance`
    above the kind's default answers 400 `importance is set by the operator`.
    Before, both were accepted and stored the record GLOBAL. A full-tenant
    token (it writes global and reads the whole tenant) keeps both.
- **Other changes a caller can notice.** A scoped `forget` or `invalidate`
  counts only the caller's own rows, and answers 400 `record not found in this
  identity's write scope` when the caller holds no live row of the uri (E1).
  A scoped `link` needs an endpoint the caller owns outright and a token
  `rel` (E5). Engine guard refusals answer 400 with their reason, not 500. A
  full-tenant token's `X-DM-Write-Scope` now stamps its writes; before, the
  header was ignored. Recall returns one hit per uri. A capped adapter token
  can no longer write global or outside its cap. The MCP `reindex_links` tool
  needs a full-tenant token.
- **Licence.** dm-lite is licensed under the Apache License 2.0 from this
  version, with attribution in `NOTICE`. Versions up to and including 0.3.6 were
  published under MIT.
- **Test data.** Test fixtures and code comments that named real organisations,
  projects, hosts and people now use invented data. No runtime behaviour
  changes.
- **Security contact.** `SECURITY.md` routes reports through private GitHub
  security advisories only; the personal email fallback is gone.

### Security

- Dependency updates for two advisories: h2 0.4.19 (RUSTSEC-2026-0258,
  unbounded empty DATA frames) and rustls 0.23.45 (RUSTSEC-2026-0285, TLS 1.3
  handshake messages accepted across encryption levels), with aws-lc-rs,
  aws-lc-sys and rustls-webpki following rustls. Also anyhow 1.0.104
  (RUSTSEC-2026-0190, `Error::downcast_mut` unsoundness), memmap2 0.9.11
  (RUSTSEC-2026-0186) and chacha20 0.10.2.
- Accepted, no fixed release exists: `number_prefix` (RUSTSEC-2025-0119),
  `paste` (RUSTSEC-2024-0436) and `rustls-pemfile` (RUSTSEC-2025-0134) are
  unmaintained. `cargo audit` reports no vulnerability.

### Upgrade notes

- **Order.** Upgrade servers before relying on `--scope`: a 0.3.7 client
  refuses `--scope` against an older server and writes nothing. Clients
  without `--scope` work against either version, and 0.3.3 and 0.3.6 clients
  work against a 0.3.7 server unchanged, except where 0.3.7 refuses on
  purpose: writes on a tenant with the explicit-write-scope policy (they
  cannot name a scope), and the E7 levers for write-restricted identities.
- **First start.** Expect the vector migration to take about half a second
  per scoped record per tenant, logged as it runs. Requests to a tenant under
  migration wait for it; a client with a short timeout may fail its first
  request to that tenant and succeed once the migration is done.
- **Rollback.** An older dmem writes scoped saves into the global vector slot
  again and does not see the per-scope slots, and nothing detects that
  afterwards. After running an older dmem on the same data, delete each
  `<tenant>.scope-slots` marker before starting 0.3.7 again, so the migration
  re-runs.
