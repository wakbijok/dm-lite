# Releasing dm-lite

How a dm-lite release is built, packaged, signed and published. It is run by hand on the
release machine. The steps in `daimon-docs` (`dm-lite/releasing.md`) are older and say to run
a plain `rsign sign`; where they differ from this page, this page is right, because it matches
the scripts in this repository.

## What a release is

One archive per target, `dmem-vX.Y.Z-<target triple>.tar.gz`, with its detached minisign
signature `<archive>.minisig` and a `SHA256SUMS` file. `dmem upgrade` looks on the release for
exactly `dmem-<tag>-<target triple>.tar.gz` and its `.minisig`, so keep that name.

The archive is flat: its members sit at its root, with no enclosing directory and no `./`
entry, because `dmem upgrade` and the README's `tar xzf` look for `dmem` there. For
`x86_64-unknown-linux-gnu` the members are exactly:

| Member | What it is |
|---|---|
| `dmem` | the binary, built with `--features dist` |
| `libzvec_c_api.so` | the native vector library, beside the binary (its RUNPATH is `$ORIGIN`) |
| `LICENSE`, `NOTICE` | the project licence and the notices NOTICE carries (zvec, Apache Arrow, AWS-LC) |
| `THIRD-PARTY-aws-lc.txt` | the AWS-LC licence text, the licence file of the locked `aws-lc-sys` |
| `THIRD_PARTY_LICENSES.md` | the licence text of every Rust crate compiled into the `dist` build (scope below) |
| `README.md` | the README |

Every archive is signed with the daimon series release key, id `CD4359A138D28837`, public key
`RWQ3iNI4oVlDzaiCJtUwwBiPjx2dcWaDHxh00Pw0lQbluZ1V33c0+gyR` (compiled into `dmem` in
`src/upgrade.rs`, `RELEASE_PUBKEYS`). The secret key lives only on the release machine, at
`~/.config/dmem-release/dm-lite-release-2.key` (0600); it is never committed, printed, copied
or uploaded, and its passphrase is typed into rsign's prompt, never into a command line.

## Two lanes, one layout

- **The hand lane**, the one in use: build the binary on the release machine, make the archive
  with `scripts/package-release.sh`, sign it, upload it. `contrib/sign-release.py TAG
  --packaged DIR` signs and uploads archives from the script's output directory, and refuses
  any archive on the release that is not byte-identical to the file the script wrote there.
- **The workflow lane**: `.github/workflows/release.yml` builds four targets from a pushed tag.
  It runs only where the tag is pushed to GitHub, and pushing there is disabled today. Its
  archives have the same members as the hand lane's (it also copies `THIRD-PARTY-aws-lc.txt`
  and `THIRD_PARTY_LICENSES.md`, and fails when either is missing), but they are not
  byte-identical to the hand lane's: another binary, another archiver. `contrib/sign-release.py
  TAG` without `--packaged` checks them against the workflow's own build artifact.

Use one lane for a given tag. A release is signed only after the signer has checked that each
archive on it is the file its lane produced.

## 1. Gate

On the release commit, with a clean tree:

```sh
cargo test --features dist        # three consecutive runs
cargo audit                       # no new vulnerabilities; document any accepted ones in the notes
cargo clippy --features dist --all-targets
scripts/test-package-release.sh   # the packaging script, on stand-in files
python3 contrib/test_sign_release.py
```

`cargo test --features dist` is the gate, not default features: the default build leaves out
the code (`dmem upgrade` and its fence among it) that a release ships.

**A host without network.** Cargo cannot resolve the locked dependencies offline unless every
`.crate` file of `Cargo.lock` is in its cache. On a host with network, in a checkout of the
release commit, run `cargo fetch --locked` (or `cargo vendor --locked`) and copy the `.crate`
files (or the vendor directory, with the source replacement `cargo vendor` prints) to the
release machine. Cargo checks each one against the checksum in `Cargo.lock`. Do not rebuild a
vendor directory from `~/.cargo/registry/src`: those unpacked sources cannot be checked against
`Cargo.lock` once their `.crate` files are gone.

The build also needs the native library. `zvec-sys` downloads `libzvec_c_api` from the zvec
release page unless `ZVEC_LIB_DIR` names a directory that already holds it; set
`ZVEC_AUTO_BUILD=0` too so that a missing library fails instead of being downloaded or built.
The `libzvec_c_api.so` in the 0.3.8 archive is the zvec 0.5.0 library and can be reused.

## 2. Version and changelog

Bump `version` in `Cargo.toml`, then refresh the lock file for this package only:

```sh
cargo update --workspace --offline
git diff Cargo.lock    # only dm-lite's own version may change
```

Rename the `Unreleased` section of `CHANGELOG.md` to `## X.Y.Z - YYYY-MM-DD`. The packaging
script refuses a version with no such section, and a `Cargo.lock` that does not carry the
version.

**Licence notices.** Make the notice again at every version bump (it names dm-lite's own
version among the crates it lists, so a bump alone makes it stale) and whenever a dependency
changed, on a host that has every crate of `Cargo.lock` in its cargo cache (cargo-about 0.9.2 reads each crate's manifest and
licence files), then read the diff and commit it:

```sh
scripts/package-release.sh --write-notice
```

Its scope is stated at the top of the file and is not "every crate in `Cargo.lock`": it is the
crates compiled into `dmem` built with `--features dist`, for the four release targets
(`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`,
`x86_64-pc-windows-msvc`), without dev- and build-dependencies. `--fail` makes cargo-about stop
when it cannot read, synthesise or clarify a licence expression for any crate in that scope, so
a crate with no licence data cannot pass silently. When it stops, fix it in `about.toml` with a
clarification that names the licence file and its checksum; do not loosen `accepted` to get
past it. The packaging script makes the file again at every release and refuses to package
when the committed one differs.

If `Cargo.lock` locks another `aws-lc-sys`, take `aws-lc/LICENSE` from that crate (in the
cargo cache, once the `.crate` file's sha256 matches the checksum in `Cargo.lock`), commit it
as `THIRD-PARTY-aws-lc.txt`, and move `AWS_LC_SYS_VERSION` and `AWS_LC_LICENSE_SHA256` in
`scripts/package-release.sh`. The script refuses to package until they agree.

Commit as `release: X.Y.Z - ...`.

## 3. Build

On the release machine, a Linux x86_64 host with glibc at least as old as the oldest host the
archive should run on (the binary needs the glibc of its build host; the 0.3.8 binary needs
2.39):

```sh
export CARGO_TARGET_DIR=$(mktemp -d /var/tmp/dmem-build.XXXXXX)     # fresh: nothing left over
export ZVEC_LIB_DIR=/path/to/the/directory/holding/libzvec_c_api.so
export ZVEC_AUTO_BUILD=0
export CARGO_INCREMENTAL=0
export CFLAGS="-ffile-prefix-map=$PWD=/dm-lite -ffile-prefix-map=${CARGO_HOME:-$HOME/.cargo}=/cargo"
export CXXFLAGS="$CFLAGS"
export RUSTFLAGS="--remap-path-prefix=$PWD=/dm-lite \
  --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo \
  --remap-path-prefix=$(rustc --print sysroot)=/rust-sysroot"
cargo build --release --locked --features dist
```

Why each line: the 0.3.8 binary has the builder's target directory in its RUNPATH
(`build.rs` adds the `zvec-prebuilt` directory of the build tree when it finds one, so a
binary can run in place) and the builder's cargo directory in its panic messages, so two builds
in different directories cannot give the same bytes. A fresh `CARGO_TARGET_DIR` with `ZVEC_LIB_DIR` set
leaves no `zvec-prebuilt` directory to find, and the remapping replaces the three directories
the compiler would otherwise name (a toolchain with the `rust-src` component names its own
directory under `~/.rustup`, which the sysroot line covers). `RUSTFLAGS` reaches only the Rust
compiler: the C sources that `aws-lc-sys` compiles keep their `__FILE__` paths under the cargo
registry unless `CFLAGS` maps them too (the first 0.4.0 build had them and was refused). The packaging script checks the
result and refuses a binary that still has either.

The binary is `$CARGO_TARGET_DIR/release/dmem`. The same compiler, flags and dependencies give
the same binary; a different toolchain does not promise it. What the script makes reproducible
is the archive, from a given binary.

## 4. Package

```sh
scripts/package-release.sh --binary $CARGO_TARGET_DIR/release/dmem \
    --lib $ZVEC_LIB_DIR/libzvec_c_api.so          # --out DIR; default ~/staging/dm-lite-vX.Y.Z
```

It refuses a dirty tree (untracked files included), a version with no changelog section, a tag
on another commit, an existing archive, a binary that is not a 64-bit x86-64 ELF, whose RUNPATH
is not exactly `$ORIGIN`, that embeds a path under `/home`, `/Users` or `/root`, that is not the
`dist` build (it must need `libzvec_c_api.so` and have the `setup`, `serve`, `service`,
`upgrade`, `login` and `ui` commands) or whose `--version` is not `dmem X.Y.Z`, an AWS-LC text
or notice that is not current, and a library whose soname is not `libzvec_c_api.so`. It then
writes `dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`, `SHA256SUMS` (the archive) and `COMMIT`
(the commit) into the output directory, and prints the binary's sha256 and the rsign command.
It never builds, signs, tags, uploads or pushes.

Same inputs give the same bytes: members in the byte order of an explicit list, owner 0, fixed
modes, the commit's time, gzip without name or timestamp. To check, delete the archive and run
it again (or run it from another checkout of the same commit) and compare `sha256sum`;
`scripts/test-package-release.sh` does this on stand-in files, with another checkout path,
umask, time zone and input file times.

## 5. Sign (the maintainer)

The signature's trusted comment must read `file:<archive>`, a TAB, then `version:X.Y.Z`. Run
rsign from the output directory with the archive's bare name, so the comment names the file:

```sh
cd ~/staging/dm-lite-vX.Y.Z
rsign sign -s ~/.config/dmem-release/dm-lite-release-2.key \
    -x dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.minisig \
    -t $'file:dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz\tversion:X.Y.Z' \
    dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
```

(The script prints this command with the names filled in, and `contrib/sign-release.py` signs
with the same comment.)

Why a tab. `dmem upgrade` splits the signed comment on tabs and needs a field equal to
`file:<archive>`, which is what rsign's default comment gives it
(`timestamp:...<TAB>file:<archive><TAB>prehashed`). The hub's own tooling writes
`file:<asset> version:X.Y.Z` with a space; its parser takes a tab or a space, but dmem's
verifier takes a tab only, so a release signed with the hub's literal form would fail
`dmem upgrade` on every installed dmem with "signature is for a different file". The
`version:` field is for readers that need one (the hub's updater refuses a signature without
it); `dmem upgrade` already ties the version through the archive name, which holds it. The 0.3.8
signature has rsign's default comment and no version field.

Signing is mandatory: `dmem upgrade` refuses an unsigned release, and so does the installer.
Never publish an archive without its `.minisig`. If the key is suspected to be compromised,
generate a new pair, add its public key to `RELEASE_PUBKEYS` in `src/upgrade.rs`, and ship that
as a release signed by the old key so that existing installs can verify their last hop.

## 6. Verify

```sh
rsign verify -P RWQ3iNI4oVlDzaiCJtUwwBiPjx2dcWaDHxh00Pw0lQbluZ1V33c0+gyR \
    -x dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz.minisig \
    dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
sha256sum -c SHA256SUMS
tar -tzf dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz    # seven members, no ./ entry
```

The first must print `Signature and comment signature verified` and a trusted comment of
`file:<archive>`, a tab, `version:X.Y.Z`. Record in the release notes: the version, the commit,
the archive's sha256, the binary's sha256, the key id, and the gate results.

## 7. Tag and push

```sh
git tag -a vX.Y.Z -m "dm-lite X.Y.Z" <release commit>
git rev-parse vX.Y.Z^{commit}    # must equal the commit in COMMIT
git push staging main vX.Y.Z
```

The remote `staging` is the GitLab project `daimon/dm-lite` on the staging GitLab host (`$GITLAB_HOST`).
The `github` remote has pushing disabled; do not enable or use it without the maintainer's decision.

## 8. Upload the asset and its signature

Upload the archive, its `.minisig` and `SHA256SUMS` to the generic package registry of the
GitLab project, then create the GitLab release for the tag with links to those three files. Use
a token with `api` scope from the maintainer's own store; it is never written to a file in a repository or
put on a command line (the `-H @-` form reads the header from standard input, so `ps` cannot
show it):

```sh
read -rs GITLAB_TOKEN    # paste, Enter
GITLAB_HOST=gitlab.example.org    # the staging GitLab host
V=X.Y.Z
N=dmem-v$V-x86_64-unknown-linux-gnu
API=https://$GITLAB_HOST/api/v4/projects/daimon%2Fdm-lite
PKG=$API/packages/generic/dm-lite/$V
for f in $N.tar.gz $N.tar.gz.minisig SHA256SUMS; do
    printf 'PRIVATE-TOKEN: %s\n' "$GITLAB_TOKEN" |
        curl --fail-with-body -sS -H @- --upload-file "$f" "$PKG/$f"
done
```

Then the release, with the notes in `notes.md` (the changelog section plus the record from step 6):

```sh
jq -n --arg v "$V" --arg n "$N" --arg pkg "$PKG" --rawfile notes notes.md '{
  tag_name: ("v" + $v), name: ("dm-lite " + $v), description: $notes,
  assets: {links: [
    {name: ($n + ".tar.gz"),         url: ($pkg + "/" + $n + ".tar.gz"),         link_type: "package"},
    {name: ($n + ".tar.gz.minisig"), url: ($pkg + "/" + $n + ".tar.gz.minisig"), link_type: "other"},
    {name: "SHA256SUMS",             url: ($pkg + "/SHA256SUMS"),                link_type: "other"}]}}' >release.json
printf 'PRIVATE-TOKEN: %s\n' "$GITLAB_TOKEN" |
    curl --fail-with-body -sS -H @- -H 'Content-Type: application/json' \
        --data @release.json "$API/releases"
unset GITLAB_TOKEN
```

Check that the release page lists the three files and that a download of the archive and the
`.minisig` verifies as in step 6.

`dmem upgrade` reads GitHub Releases (`wakbijok/dm-lite`), not GitLab, so a release uploaded
only to GitLab is not seen by it. On an installer-managed host `dmem upgrade` refuses anyway
(see [managed-install.md](managed-install.md)); the installer takes the archive, the `.minisig`
and `SHA256SUMS` from the GitLab release. Publishing to GitHub is a separate decision: if the maintainer
makes it, push the tag there, upload the archive, and run `contrib/sign-release.py vX.Y.Z
--packaged DIR`, which signs each archive on the GitHub release with the comment above and
uploads the signatures.

## 9. After the release

Put the archive's sha256, the binary's sha256, the version and the key id in the installer's
manifest and the rollout record. Keep the output directory until the release is deployed: it is
the source for an offline install.
