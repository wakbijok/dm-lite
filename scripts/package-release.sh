#!/usr/bin/env bash
# Package a dm-lite release: the flat archive that `dmem upgrade`, an installer and a manual
# install all unpack, made the same way every time. Run by hand on the release machine, on a
# clean checkout of the commit to be tagged, after the gate (docs/releasing.md).
#
#   scripts/package-release.sh --binary DMEM --lib LIBZVEC_C_API [--out DIR]
#   scripts/package-release.sh --write-notice
#
# It packages a binary that is already built; it never builds. docs/releasing.md has the
# build command (a fresh target directory, source paths remapped, the native library given
# by ZVEC_LIB_DIR). It checks the binary it is given, stages the files, and writes
# dmem-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz, SHA256SUMS and COMMIT into DIR. DIR defaults
# to ~/staging/dm-lite-vX.Y.Z and should be outside the checkout. It never signs, tags,
# uploads or pushes; it prints the rsign command for the signature.
#
# The archive is flat: the files sit at its root, with no enclosing directory and no ./
# entry, because `dmem upgrade` and the README's `tar xzf` both look for dmem there. Its
# members are exactly:
#
#   LICENSE NOTICE README.md THIRD-PARTY-aws-lc.txt THIRD_PARTY_LICENSES.md dmem libzvec_c_api.so
#
# It refuses to package
#   - a dirty git tree (untracked files included), a version with no CHANGELOG section, a
#     Cargo.lock that does not carry the version, or a tag that is on another commit;
#   - a binary that is not a Linux x86_64 ELF, whose RUNPATH is anything but $ORIGIN, that
#     embeds a build path under /home, /Users or /root, that is not the dist build (no
#     libzvec_c_api dependency, or no upgrade, serve, setup, login, ui and service
#     commands), or whose --version is not "dmem X.Y.Z" for the checkout's version;
#   - a THIRD-PARTY-aws-lc.txt that is not the AWS-LC licence file of the aws-lc-sys version
#     Cargo.lock locks, and a THIRD_PARTY_LICENSES.md that is not what cargo-about generates
#     now (a stale notice fails the release);
#   - an archive that already exists (a signed asset is never rebuilt in place).
#
# Same inputs, same bytes: sorted members from an explicit list, owner 0, fixed modes, the
# commit's time, gzip without a name or timestamp. Neither the checkout's path, the user's
# umask or time zone, nor the input files' own times and modes reach the archive.
set -euo pipefail
umask 022
export LC_ALL=C TZ=UTC
cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-gnu
PUBKEY='RWQ3iNI4oVlDzaiCJtUwwBiPjx2dcWaDHxh00Pw0lQbluZ1V33c0+gyR'
# Printed, never read: the signing key stays on the release machine.
# shellcheck disable=SC2088 # the tilde is for the printed command
SIGNING_KEY='~/.config/dmem-release/dm-lite-release-2.key'
# What the archive must hold, in the byte order tar writes them in.
MEMBERS=(LICENSE NOTICE README.md THIRD-PARTY-aws-lc.txt THIRD_PARTY_LICENSES.md dmem libzvec_c_api.so)
# The aws-lc-sys release whose licence file THIRD-PARTY-aws-lc.txt is, and that file's sha256.
# When Cargo.lock locks another aws-lc-sys, take the file from the new crate (its
# aws-lc/LICENSE), commit it, and move both values here.
AWS_LC_SYS_VERSION=0.45.0
AWS_LC_LICENSE_SHA256=977c541bd25ffe36975dde25c028c0cd15ddf3d08243983b58e8a6a3d9447523
CARGO_ABOUT_VERSION=0.9.2
# The signed trusted comment: two fields separated by a TAB, because `dmem upgrade` splits the
# comment on tabs and needs a field equal to file:<archive>. The hub's own tooling writes a
# space; a signature made with a space is refused by every installed dmem. contrib/sign-release.py
# writes the same comment.
COMMENT_FORMAT='file:%s\tversion:%s'

fail() {
    echo "package-release: $*" >&2
    exit 1
}

# The command that makes THIRD_PARTY_LICENSES.md (also in about.toml). Offline and locked, so
# its result depends only on Cargo.lock, about.toml and the crates in the cargo cache.
notice() {
    cargo about generate --frozen --fail -c about.toml \
        --no-default-features --features dist -o "$1" about.md.hbs
}

need_cargo_about() {
    command -v cargo-about >/dev/null ||
        fail "cargo-about is missing: cargo install --locked cargo-about@$CARGO_ABOUT_VERSION --features cli"
    local v
    v=$(cargo about --version) || fail "cannot run cargo about --version"
    [[ $v == "cargo-about $CARGO_ABOUT_VERSION" ]] ||
        fail "cargo-about is '$v'; the notice is made with cargo-about $CARGO_ABOUT_VERSION"
}

BINARY= LIB= OUT= WRITE_NOTICE=
while [[ $# -gt 0 ]]; do
    case $1 in
        --binary) [[ $# -ge 2 ]] || fail "--binary needs a file"; BINARY=$2; shift 2 ;;
        --binary=*) BINARY=${1#*=}; shift ;;
        --lib) [[ $# -ge 2 ]] || fail "--lib needs a file"; LIB=$2; shift 2 ;;
        --lib=*) LIB=${1#*=}; shift ;;
        --out) [[ $# -ge 2 ]] || fail "--out needs a directory"; OUT=$2; shift 2 ;;
        --out=*) OUT=${1#*=}; shift ;;
        --write-notice) WRITE_NOTICE=1; shift ;;
        -h|--help) sed -n '2,/^set /p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) fail "unknown argument $1 (see --help)" ;;
    esac
done

work=$(mktemp -d)
trap 'rm -rf "$work"; [[ -z ${ARCHIVE:-} ]] || rm -f "$ARCHIVE.part"' EXIT

# --- the notice, on request: generate it where the whole crate set is in the cargo cache ----

if [[ -n $WRITE_NOTICE ]]; then
    [[ -z $BINARY && -z $LIB && -z $OUT ]] || fail "--write-notice takes no other argument"
    need_cargo_about
    notice "$work/THIRD_PARTY_LICENSES.md"
    cp "$work/THIRD_PARTY_LICENSES.md" THIRD_PARTY_LICENSES.md
    echo "package-release: wrote THIRD_PARTY_LICENSES.md; read the diff and commit it"
    exit 0
fi

[[ -n $BINARY ]] || fail "--binary is required (see --help)"
[[ -n $LIB ]] || fail "--lib is required (see --help)"
[[ -f $BINARY ]] || fail "no such file: $BINARY"
[[ -f $LIB ]] || fail "no such file: $LIB"
[[ "$(uname -s)/$(uname -m)" == Linux/x86_64 ]] || fail "package on Linux x86_64 for $TARGET"
[[ $(tar --version) == *'GNU tar'* ]] || fail "GNU tar is needed (--owner, --mtime and --mode)"
command -v readelf >/dev/null || fail "readelf is needed (binutils)"

# --- what is being released ---------------------------------------------------

dirty=$(git status --porcelain --untracked-files=all)
[[ -z $dirty ]] || fail "the git tree is dirty; commit or remove these first:
$dirty"
SHA=$(git rev-parse HEAD)
VERSION=$(sed -n '/^\[package\]/,/^\[/s/^version = "\([^"]*\)"$/\1/p' Cargo.toml)
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || fail "cannot read the version from Cargo.toml"
grep -q "^## ${VERSION//./\\.} " CHANGELOG.md || fail "CHANGELOG.md has no '## $VERSION - <date>' section"
locked=$(awk '$0 == "name = \"dm-lite\"" { getline; print; exit }' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p')
[[ $locked == "$VERSION" ]] || fail "Cargo.lock holds dm-lite '$locked', not $VERSION; refresh it and commit"
if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
    [[ $(git rev-parse "v$VERSION^{commit}") == "$SHA" ]] ||
        fail "tag v$VERSION exists and points at another commit"
fi

NAME=dmem-v$VERSION-$TARGET
OUT=${OUT:-$HOME/staging/dm-lite-v$VERSION}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd -P)
ARCHIVE=$OUT/$NAME.tar.gz
[[ ! -e $ARCHIVE ]] || fail "$ARCHIVE exists; remove it and its .minisig to package again"

echo "package-release: dm-lite $VERSION at ${SHA:0:12} -> $OUT"

# --- the licence files that travel with the binary ------------------------------

for f in LICENSE NOTICE README.md THIRD-PARTY-aws-lc.txt; do
    git ls-files --error-unmatch -- "$f" >/dev/null 2>&1 || fail "$f is not tracked by git"
done
git ls-files --error-unmatch -- THIRD_PARTY_LICENSES.md >/dev/null 2>&1 ||
    fail "THIRD_PARTY_LICENSES.md is not committed: generate it with scripts/package-release.sh --write-notice on a host that has every crate of Cargo.lock, review it and commit it"

sys=$(awk '$0 == "name = \"aws-lc-sys\"" { getline; print }' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p')
[[ $(wc -l <<<"$sys") -eq 1 && -n $sys ]] || fail "Cargo.lock must lock exactly one aws-lc-sys; it locks: ${sys:-none}"
[[ $sys == "$AWS_LC_SYS_VERSION" ]] ||
    fail "Cargo.lock locks aws-lc-sys $sys but THIRD-PARTY-aws-lc.txt is from $AWS_LC_SYS_VERSION: take aws-lc/LICENSE from the new crate, commit it as THIRD-PARTY-aws-lc.txt, and update AWS_LC_SYS_VERSION and AWS_LC_LICENSE_SHA256 in this script"
[[ $(sha256sum THIRD-PARTY-aws-lc.txt | cut -d ' ' -f 1) == "$AWS_LC_LICENSE_SHA256" ]] ||
    fail "THIRD-PARTY-aws-lc.txt is not the aws-lc-sys $AWS_LC_SYS_VERSION licence file (sha256 $AWS_LC_LICENSE_SHA256)"

need_cargo_about
notice "$work/THIRD_PARTY_LICENSES.md"
cmp -s "$work/THIRD_PARTY_LICENSES.md" THIRD_PARTY_LICENSES.md ||
    fail "THIRD_PARTY_LICENSES.md is out of date: regenerate it with scripts/package-release.sh --write-notice, review it and commit it"

# --- stage ---------------------------------------------------------------------

stage=$work/stage
mkdir "$stage"
for f in LICENSE NOTICE README.md THIRD-PARTY-aws-lc.txt THIRD_PARTY_LICENSES.md; do
    install -m 0644 "$f" "$stage/$f"
done
install -m 0755 "$BINARY" "$stage/dmem"
install -m 0755 "$LIB" "$stage/libzvec_c_api.so"

# --- check the binary and the library ---------------------------------------------

elf_field() { readelf -h "$1" 2>/dev/null | sed -n "s/^ *$2: *//p"; }
for f in dmem libzvec_c_api.so; do
    [[ $(elf_field "$stage/$f" Class) == ELF64 ]] || fail "$f is not a 64-bit ELF file"
    [[ $(elf_field "$stage/$f" Machine) == *X86-64* ]] || fail "$f is not for x86-64"
done

dyn=$(readelf -d "$stage/dmem")
rpath=$(sed -n 's/.*(RPATH).*\[\(.*\)\]$/\1/p' <<<"$dyn")
runpath=$(sed -n 's/.*(RUNPATH).*\[\(.*\)\]$/\1/p' <<<"$dyn")
[[ -z $rpath ]] || fail "dmem has a DT_RPATH ($rpath); the archive needs a RUNPATH of exactly \$ORIGIN"
[[ $runpath == '$ORIGIN' ]] ||
    fail "dmem's RUNPATH is [${runpath:-none}], not exactly [\$ORIGIN]: it was built where a build directory was added to it (see docs/releasing.md: a fresh CARGO_TARGET_DIR and ZVEC_LIB_DIR)"
grep -q '(NEEDED).*\[libzvec_c_api\.so\]' <<<"$dyn" ||
    fail "dmem does not need libzvec_c_api.so: it was not built with --features dist"
soname=$(readelf -d "$stage/libzvec_c_api.so" | sed -n 's/.*(SONAME).*\[\(.*\)\]$/\1/p')
[[ $soname == libzvec_c_api.so ]] || fail "--lib is not libzvec_c_api.so (its soname is '${soname:-none}')"

if grep -a -q -E '/(home|Users|root)/' "$stage/dmem"; then
    paths=$(grep -a -o -E '/(home|Users|root)/[[:alnum:]_.-]+' "$stage/dmem" | sort -u | head -3 | tr '\n' ' ')
    fail "dmem embeds a build path ($paths...): build with --remap-path-prefix for the source and cargo paths (docs/releasing.md), so the bytes do not depend on where it was built"
fi

run_dmem() { env -i PATH=/usr/bin:/bin HOME="$work" "$stage/dmem" "$@"; }
reported=$(run_dmem --version) || fail "dmem --version does not run (is libzvec_c_api.so loadable beside it?)"
[[ $reported == "dmem $VERSION" ]] || fail "dmem --version says '$reported', not 'dmem $VERSION'"
usage_text=$(run_dmem --help) || fail "dmem --help does not run"
for c in setup serve service upgrade login ui; do
    grep -q "^  $c\( \|\$\)" <<<"$usage_text" || fail "dmem has no '$c' command: it was not built with --features dist"
done

# --- archive -----------------------------------------------------------------

sorted=$(printf '%s\n' "${MEMBERS[@]}" | sort)
[[ $sorted == "$(printf '%s\n' "${MEMBERS[@]}")" ]] || fail "internal: MEMBERS is not in byte order"

mtime=$(git log -1 --format=%ct HEAD)
# An explicit list, not a directory: no ./ entry and no enclosing folder.
tar --owner=0 --group=0 --numeric-owner --mtime="@$mtime" \
    --mode='u+rwX,go+rX,go-w' --format=gnu -C "$stage" -cf - "${MEMBERS[@]}" |
    gzip -n -9 >"$ARCHIVE.part"
mv "$ARCHIVE.part" "$ARCHIVE"

listing=$(tar -tzf "$ARCHIVE")
[[ $listing == "$(printf '%s\n' "${MEMBERS[@]}")" ]] || fail "the archive holds unexpected members:
$listing"
odd=$(tar --numeric-owner -tvzf "$ARCHIVE" |
    awk '$2 != "0/0" || ($1 != "-rw-r--r--" && $1 != "-rwxr-xr-x") { print }')
[[ -z $odd ]] || fail "the archive has members with another owner or mode:
$odd"

(
    cd "$OUT"
    sha256sum "$NAME.tar.gz"
) >"$OUT/SHA256SUMS"
printf '%s\n' "$SHA" >"$OUT/COMMIT"
bin_sha=$(sha256sum "$stage/dmem" | cut -d ' ' -f 1)

comment=${COMMENT_FORMAT/'%s'/$NAME.tar.gz}
comment=${comment/'%s'/$VERSION}

cat <<EOF

package-release: done
  archive       $ARCHIVE
  sums          $OUT/SHA256SUMS
  commit        $OUT/COMMIT ($SHA)
  binary sha256 $bin_sha
  version       $reported

Sign it on the release machine (the maintainer's; the key never leaves it). Run rsign from the output
directory so the signed comment names the archive file, with this trusted comment: a TAB
between the two fields, not a space, because dmem upgrade splits on tabs:

  cd $OUT
  rsign sign -s $SIGNING_KEY -x $NAME.tar.gz.minisig \\
      -t \$'$comment' $NAME.tar.gz

(contrib/sign-release.py v$VERSION --packaged $OUT signs with the same comment and uploads.)
Then verify, and check that the trusted comment shows file:$NAME.tar.gz, a tab, and version:$VERSION:

  rsign verify -P $PUBKEY -x $NAME.tar.gz.minisig $NAME.tar.gz

Then tag v$VERSION at ${SHA:0:12} and publish (docs/releasing.md).
EOF
