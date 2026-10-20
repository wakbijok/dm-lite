#!/usr/bin/env bash
# Tests for scripts/package-release.sh. Run from anywhere:
#
#   scripts/test-package-release.sh
#
# It needs bash, git, GNU tar, gzip, readelf and rustc, and nothing from the network. It
# builds a small stand-in for dmem (and for libzvec_c_api.so) with rustc, so the script's checks
# run on real ELF files, makes a throwaway git repository from the files the script reads (the
# working tree's own copies, committed with a fixed date), and puts a stand-in cargo-about
# first in PATH. It never touches this checkout. What it shows:
#
#   - the same inputs give the same archive bytes: twice in one checkout, from two checkouts
#     at different paths, and with another umask, time zone, home directory, and another
#     mode and time on the input files;
#   - the archive is flat, with exactly the listed members, owner 0, and no ./ entry;
#   - the script refuses each bad input it promises to refuse, and says why;
#   - the signing command it prints carries the trusted comment dmem upgrade accepts: the
#     archive name, a tab, and the version.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd -P)
T=$(mktemp -d)
trap 'chmod -R u+rwX "$T" 2>/dev/null || true; rm -rf "$T"' EXIT
export TMPDIR=$T
# one test changes HOME; the toolchain proxies must keep finding the toolchains
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
failures=0
checks=0

ok() { checks=$((checks + 1)); echo "ok   $*"; }
bad() { checks=$((checks + 1)); failures=$((failures + 1)); echo "FAIL $*"; }

# expect_ok NAME CMD...: CMD succeeds.
expect_ok() {
    local name=$1 out
    shift
    if out=$("$@" 2>&1); then ok "$name"; else bad "$name: $out"; fi
}

# expect_refusal NAME PATTERN CMD...: CMD fails and its output matches PATTERN.
expect_refusal() {
    local name=$1 pat=$2 out
    shift 2
    if out=$("$@" 2>&1); then
        bad "$name: it was not refused"
    elif grep -q -- "$pat" <<<"$out"; then
        ok "$name"
    else
        bad "$name: refused, but not as '$pat': $out"
    fi
}

assert_eq() {
    if [[ $2 == "$3" ]]; then ok "$1"; else bad "$1: '$2' is not '$3'"; fi
}

VERSION=$(sed -n '/^\[package\]/,/^\[/s/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml")
NAME=dmem-v$VERSION-x86_64-unknown-linux-gnu
SCRIPT=scripts/package-release.sh

# --- stand-ins for the native library and the binary ----------------------------------

# Source paths in the compiler's own library code name the toolchain's directory when it has
# the library sources installed; remap them, as the release build does (docs/releasing.md).
REMAP=(--remap-path-prefix "$(rustc --print sysroot)=/rust-sysroot")

mkdir -p "$T/fx"
cd "$T/fx"
cat >lib.rs <<'EOF'
#[no_mangle]
pub extern "C" fn zvec_stub() -> i32 {
    7
}
EOF
rustc --edition 2021 -C strip=debuginfo "${REMAP[@]}" --crate-type cdylib -C link-arg=-Wl,-soname,libzvec_c_api.so \
    lib.rs -o libzvec_c_api.so 2>/dev/null

# main_rs VERSION_TEXT HELP_COMMANDS [PATH_STRING]
main_rs() {
    cat <<EOF
extern "C" {
    fn zvec_stub() -> i32;
}
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let v = unsafe { zvec_stub() };
    match a.get(1).map(String::as_str) {
        Some("--version") => println!("dmem $1"),
        Some("--help") => {
            println!("Commands:");
            for c in "$2".split(' ') {
                println!("  {c:<12} text");
            }
        }
        Some("--path") => println!("${3:-none}"),
        _ => std::process::exit(if v == 7 { 0 } else { 1 }),
    }
}
EOF
}
ALL_COMMANDS='setup serve service upgrade login ui bootstrap'

# build NAME VERSION_TEXT HELP_COMMANDS [PATH_STRING [EXTRA_LINK_ARG]]
build() {
    main_rs "$2" "$3" "${4:-}" >"$1.rs"
    local extra=()
    [[ -z ${5:-} ]] || extra=(-C "link-arg=$5")
    rustc --edition 2021 -C strip=debuginfo "${REMAP[@]}" -L . -l dylib=zvec_c_api \
        -C 'link-arg=-Wl,-rpath,$ORIGIN' "${extra[@]}" "$1.rs" -o "$1" 2>/dev/null
}
build good "$VERSION" "$ALL_COMMANDS"
build bad-runpath "$VERSION" "$ALL_COMMANDS" "" -Wl,-rpath,/opt/build/target/release/build/zvec-prebuilt
build bad-path "$VERSION" "$ALL_COMMANDS" /home/builder/.cargo/registry/src/x/lib.rs
build bad-version 0.0.1 "$ALL_COMMANDS"
build bad-no-upgrade "$VERSION" "setup serve service login ui"
# no zvec dependency: a build without the native library
cat >plain.rs <<EOF
fn main() { if std::env::args().nth(1).as_deref() == Some("--version") { println!("dmem $VERSION"); } }
EOF
rustc --edition 2021 -C strip=debuginfo "${REMAP[@]}" -C 'link-arg=-Wl,-rpath,$ORIGIN' plain.rs -o bad-no-zvec 2>/dev/null
printf 'not an elf file\n' >bad-text
cat >lib2.rs <<'EOF'
#[no_mangle]
pub extern "C" fn other() {}
EOF
rustc --edition 2021 -C strip=debuginfo "${REMAP[@]}" --crate-type cdylib -C link-arg=-Wl,-soname,libother.so \
    lib2.rs -o libother.so 2>/dev/null

for f in good libzvec_c_api.so; do
    readelf -d "$f" >/dev/null || { echo "cannot build the stand-in $f"; exit 1; }
done
assert_eq "the stand-in binary has RUNPATH \$ORIGIN" \
    "$(readelf -d good | sed -n 's/.*(RUNPATH).*\[\(.*\)\]$/\1/p')" '$ORIGIN'
./good --version >/dev/null || { echo "the stand-in binary does not run"; exit 1; }

# --- a throwaway repository from the files the script reads -----------------------------

cd "$repo"
mkdir -p "$T/src/scripts"
for f in Cargo.toml Cargo.lock CHANGELOG.md LICENSE NOTICE README.md THIRD-PARTY-aws-lc.txt about.toml about.md.hbs; do
    cp "$f" "$T/src/$f"
done
cp scripts/package-release.sh "$T/src/scripts/"
printf 'Third-party licences (fixture)\n' >"$T/src/THIRD_PARTY_LICENSES.md"
cd "$T/src"
git init -q -b main
git add -A
GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid \
    GIT_AUTHOR_DATE='2026-10-06T12:00:00Z' GIT_COMMITTER_DATE='2026-10-06T12:00:00Z' \
    git commit -q -m fixture
git clone -q "$T/src" "$T/a/repo"
git clone -q "$T/src" "$T/b/deeper/place/repo"
cp "$T/src/THIRD_PARTY_LICENSES.md" "$T/fake-notice.md"
printf 'a different notice\n' >"$T/stale-notice.md"

# The stand-in cargo-about: reports the pinned version and "generates" the file named by -o from
# FAKE_NOTICE. CARGO_HOME is moved so cargo does not find the real one first.
mkdir -p "$T/bin" "$T/cargo-home"
cat >"$T/bin/cargo-about" <<'EOF'
#!/usr/bin/env bash
[[ $1 == about ]] || exit 64
shift
case $1 in
    --version) echo "cargo-about ${FAKE_ABOUT_VERSION:-0.9.2}" ;;
    generate)
        out=
        while [[ $# -gt 0 ]]; do
            [[ $1 == -o ]] && out=$2
            shift
        done
        [[ -n $out ]] || exit 65
        cp "$FAKE_NOTICE" "$out"
        ;;
    *) exit 66 ;;
esac
EOF
chmod +x "$T/bin/cargo-about"
export PATH="$T/bin:$PATH" CARGO_HOME="$T/cargo-home" FAKE_NOTICE="$T/fake-notice.md"

# pack CHECKOUT OUT [ARGS...]: run the script in CHECKOUT with the good inputs.
pack() {
    local co=$1 out=$2
    shift 2
    (cd "$co" && "$SCRIPT" --binary "$T/fx/good" --lib "$T/fx/libzvec_c_api.so" --out "$out" "$@")
}

# --- the same bytes ------------------------------------------------------------------

expect_ok "packages a good build" pack "$T/a/repo" "$T/out-a1"
A1=$(sha256sum "$T/out-a1/$NAME.tar.gz" | cut -d ' ' -f 1)

rm -rf "$T/out-a2"
expect_ok "packages the same build again in the same checkout" pack "$T/a/repo" "$T/out-a2"
assert_eq "the same checkout gives the same bytes" "$(sha256sum "$T/out-a2/$NAME.tar.gz" | cut -d ' ' -f 1)" "$A1"

expect_ok "packages from a checkout at another path" pack "$T/b/deeper/place/repo" "$T/out-b"
assert_eq "another checkout path gives the same bytes" "$(sha256sum "$T/out-b/$NAME.tar.gz" | cut -d ' ' -f 1)" "$A1"

# other umask, time zone, locale and home; inputs with other modes and times
mkdir -p "$T/fx2"
cp "$T/fx/good" "$T/fx2/dmem-built-elsewhere"
cp "$T/fx/libzvec_c_api.so" "$T/fx2/libzvec_c_api.so"
chmod 0700 "$T/fx2/dmem-built-elsewhere"
chmod 0600 "$T/fx2/libzvec_c_api.so"
touch -d '2001-02-03 04:05:06' "$T/fx2/dmem-built-elsewhere" "$T/fx2/libzvec_c_api.so"
other_env() {
    (
        umask 077
        export TZ=Asia/Kuala_Lumpur LC_ALL=C.UTF-8 HOME="$T/another-home"
        cd "$T/b/deeper/place/repo" && "$SCRIPT" --binary "$T/fx2/dmem-built-elsewhere" \
            --lib "$T/fx2/libzvec_c_api.so" --out "$T/out-c"
    )
}
expect_ok "packages with another umask, time zone, home and input modes and times" other_env
assert_eq "the environment and the inputs' own modes and times do not reach the bytes" \
    "$(sha256sum "$T/out-c/$NAME.tar.gz" | cut -d ' ' -f 1)" "$A1"

# --- the archive ---------------------------------------------------------------------

want=$'LICENSE\nNOTICE\nREADME.md\nTHIRD-PARTY-aws-lc.txt\nTHIRD_PARTY_LICENSES.md\ndmem\nlibzvec_c_api.so'
assert_eq "the archive is flat, with exactly the listed members and no ./ entry" \
    "$(tar -tzf "$T/out-a1/$NAME.tar.gz")" "$want"
assert_eq "every member is owned by 0/0 with mode 644 or 755" \
    "$(tar --numeric-owner -tvzf "$T/out-a1/$NAME.tar.gz" | awk '{print $1, $2}' | sort | uniq -c | tr -s ' ' | tr '\n' ';')" \
    " 5 -rw-r--r-- 0/0; 2 -rwxr-xr-x 0/0;"
assert_eq "every member carries the commit's time" \
    "$(TZ=UTC tar --numeric-owner -tvzf "$T/out-a1/$NAME.tar.gz" | awk '{print $4, $5}' | sort -u)" "2026-10-06 12:00"
assert_eq "the gzip header has no name and no time" \
    "$(od -An -tx1 -N8 "$T/out-a1/$NAME.tar.gz" | tr -d ' \n')" "1f8b080000000000"
mkdir "$T/unpacked"
tar -xzf "$T/out-a1/$NAME.tar.gz" -C "$T/unpacked"
assert_eq "dmem unpacks beside its library and runs" "$("$T/unpacked/dmem" --version)" "dmem $VERSION"
assert_eq "the notice and the AWS-LC text are the committed files" \
    "$(cd "$T/unpacked" && sha256sum THIRD-PARTY-aws-lc.txt THIRD_PARTY_LICENSES.md NOTICE | cut -d ' ' -f 1 | tr '\n' ' ')" \
    "$(cd "$T/src" && sha256sum THIRD-PARTY-aws-lc.txt THIRD_PARTY_LICENSES.md NOTICE | cut -d ' ' -f 1 | tr '\n' ' ')"
assert_eq "SHA256SUMS names the archive" "$(cat "$T/out-a1/SHA256SUMS")" "$A1  $NAME.tar.gz"
assert_eq "COMMIT holds the commit" "$(cat "$T/out-a1/COMMIT")" "$(git -C "$T/a/repo" rev-parse HEAD)"
(cd "$T/out-a1" && sha256sum -c SHA256SUMS >/dev/null) && ok "sha256sum -c accepts SHA256SUMS" || bad "sha256sum -c"

# --- the printed signing command ------------------------------------------------------

printed=$(pack "$T/a/repo" "$T/out-a3" | sed -n 's/^ *-t \(\$'"'"'.*'"'"'\) '"$NAME"'\.tar\.gz$/\1/p')
if [[ -n $printed ]]; then
    eval "got=$printed"
    assert_eq "the printed trusted comment is the archive name, a tab and the version" \
        "$got" "file:$NAME.tar.gz"$'\t'"version:$VERSION"
else
    bad "the signing command is not printed with a -t \$'...' trusted comment"
fi
rm -rf "$T/out-a3"

# --- refusals -----------------------------------------------------------------------

cd "$T/a/repo"
expect_refusal "refuses an archive that exists" "exists; remove it" pack "$T/a/repo" "$T/out-a1"
expect_refusal "refuses a missing --binary" "--binary is required" "$SCRIPT" --lib "$T/fx/libzvec_c_api.so"
expect_refusal "refuses a missing --lib" "--lib is required" "$SCRIPT" --binary "$T/fx/good"
expect_refusal "refuses an unknown argument" "unknown argument" "$SCRIPT" --frobnicate

try() { # try BINARY LIB OUTDIR
    (cd "$T/a/repo" && "$SCRIPT" --binary "$1" --lib "$2" --out "$3")
}
L=$T/fx/libzvec_c_api.so
expect_refusal "refuses a binary with an added RUNPATH" 'RUNPATH is \[$ORIGIN:/opt/build' try "$T/fx/bad-runpath" "$L" "$T/o1"
expect_refusal "refuses a binary that embeds a build path" "embeds a build path" try "$T/fx/bad-path" "$L" "$T/o2"
expect_refusal "refuses a binary of another version" "says 'dmem 0.0.1'" try "$T/fx/bad-version" "$L" "$T/o3"
expect_refusal "refuses a binary without the upgrade command" "no 'upgrade' command" try "$T/fx/bad-no-upgrade" "$L" "$T/o4"
expect_refusal "refuses a binary that does not need the native library" "does not need libzvec_c_api.so" try "$T/fx/bad-no-zvec" "$L" "$T/o5"
expect_refusal "refuses a binary that is not ELF" "not a 64-bit ELF" try "$T/fx/bad-text" "$L" "$T/o6"
expect_refusal "refuses a library of another name" "not libzvec_c_api.so" try "$T/fx/good" "$T/fx/libother.so" "$T/o7"
for o in o1 o2 o3 o4 o5 o6 o7; do
    [[ ! -e $T/$o/$NAME.tar.gz ]] || bad "a refused build left an archive in $o"
done

# a copy of the repository per refusal that needs a different tree
clone_edit() { # clone_edit NAME COMMAND...: a fresh clone in $T/NAME with COMMAND run in it and committed
    local name=$1
    shift
    git clone -q "$T/src" "$T/$name"
    (
        cd "$T/$name"
        "$@"
        git add -A
        git -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m edit
    )
}
tryin() { # tryin NAME OUTDIR
    (cd "$T/$1" && "$SCRIPT" --binary "$T/fx/good" --lib "$L" --out "$2")
}

git clone -q "$T/src" "$T/dirty"
echo stray >"$T/dirty/untracked-file"
expect_refusal "refuses a dirty tree (an untracked file)" "the git tree is dirty" tryin dirty "$T/o8"

clone_edit nochangelog sed -i 's/^## [0-9][0-9.]* - /## 9.9.9 - /' CHANGELOG.md
expect_refusal "refuses a version with no CHANGELOG section" "has no '## $VERSION - " tryin nochangelog "$T/o9"

clone_edit nonotice git rm -q THIRD_PARTY_LICENSES.md
expect_refusal "refuses a notice that is not committed" "THIRD_PARTY_LICENSES.md is not committed" tryin nonotice "$T/o10"

expect_refusal "refuses a stale notice" "THIRD_PARTY_LICENSES.md is out of date" \
    env FAKE_NOTICE="$T/stale-notice.md" bash -c 'cd "$1" && shift && "$@"' _ "$T/a/repo" "$SCRIPT" --binary "$T/fx/good" --lib "$L" --out "$T/o11"
expect_refusal "refuses another cargo-about" "the notice is made with cargo-about 0.9.2" \
    env FAKE_ABOUT_VERSION=0.8.0 bash -c 'cd "$1" && shift && "$@"' _ "$T/a/repo" "$SCRIPT" --binary "$T/fx/good" --lib "$L" --out "$T/o12"

clone_edit awsmoved sed -i '/^name = "aws-lc-sys"$/{n;s/^version = .*/version = "0.46.0"/}' Cargo.lock
expect_refusal "refuses an AWS-LC text from another aws-lc-sys than the lock" "locks aws-lc-sys 0.46.0" tryin awsmoved "$T/o13"

clone_edit awsedited sh -c 'echo extra >>THIRD-PARTY-aws-lc.txt'
expect_refusal "refuses an AWS-LC text that was edited" "is not the aws-lc-sys .* licence file" tryin awsedited "$T/o14"

clone_edit lockstale sed -i '/^name = "dm-lite"$/{n;s/^version = .*/version = "0.0.1"/}' Cargo.lock
expect_refusal "refuses a Cargo.lock that does not carry the version" "Cargo.lock holds dm-lite '0.0.1'" tryin lockstale "$T/o15"

clone_edit taggedelsewhere sh -c 'true'
git -C "$T/taggedelsewhere" tag "v$VERSION" HEAD~1
expect_refusal "refuses a tag on another commit" "points at another commit" tryin taggedelsewhere "$T/o16"

# --- the notice, on request ------------------------------------------------------------

git clone -q "$T/src" "$T/wn"
printf 'regenerated\n' >"$T/regen.md"
(cd "$T/wn" && FAKE_NOTICE="$T/regen.md" "$SCRIPT" --write-notice >/dev/null)
assert_eq "--write-notice writes THIRD_PARTY_LICENSES.md" "$(cat "$T/wn/THIRD_PARTY_LICENSES.md")" "regenerated"
expect_refusal "--write-notice takes no other argument" "takes no other argument" \
    bash -c 'cd "$1" && "$2" --write-notice --out x' _ "$T/wn" "$SCRIPT"

echo
echo "$((checks - failures)) of $checks checks passed"
[[ $failures -eq 0 ]]
