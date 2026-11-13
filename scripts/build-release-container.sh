#!/usr/bin/env bash
# Build the release binary (docs/releasing.md step 3) in the Ubuntu 24.04 image of
# contrib/release-builder, so the binary needs glibc 2.39 on any build host with Docker.
#
#   scripts/build-release-container.sh --lib-dir DIR [--out DIR]
#
# DIR holds libzvec_c_api.so (ZVEC_LIB_DIR). The checkout is mounted read-only at /dm-lite and
# must be clean. The target directory is fresh (default: a new /var/tmp/dmem-build.XXXXXX);
# the binary is <out>/release/dmem. Crates are cached in the Docker volume dm-lite-cargo.
#
# CARGO_BUILD_JOBS, when set, reaches the build inside the container and also caps the
# container at that many CPUs (docker run --cpus). The container's processes are children of
# dockerd, not of this shell, so `nice` on this script does not reach them; the cap does. The
# job count does not change the binary's bytes.
set -euo pipefail

lib_dir= out=
while [ $# -gt 0 ]; do
    case "$1" in
        --lib-dir) lib_dir=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) echo "usage: $0 --lib-dir DIR [--out DIR]" >&2; exit 2 ;;
    esac
done
[ -n "$lib_dir" ] && [ -f "$lib_dir/libzvec_c_api.so" ] || { echo "build-release: --lib-dir must hold libzvec_c_api.so" >&2; exit 2; }
lib_dir=$(cd "$lib_dir" && pwd)

root=$(git rev-parse --show-toplevel)    # the checkout to build: the current directory's
[ -z "$(git -C "$root" status --porcelain --untracked-files=all)" ] || { echo "build-release: the tree is not clean" >&2; exit 1; }

if [ -z "$out" ]; then
    out=$(mktemp -d /var/tmp/dmem-build.XXXXXX)
else
    mkdir -p "$out"
    [ -z "$(ls -A "$out")" ] || { echo "build-release: $out is not empty" >&2; exit 1; }
fi
out=$(cd "$out" && pwd)

limit=()
if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
    [[ $CARGO_BUILD_JOBS =~ ^[1-9][0-9]{0,2}$ ]] || { echo "build-release: CARGO_BUILD_JOBS must be a whole number from 1 to 999" >&2; exit 2; }
    limit=(-e "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS" --cpus "$CARGO_BUILD_JOBS")
fi

# The image comes from the script's own checkout, so an older tag (without the Dockerfile) can be built.
ctx=$(cd "$(dirname "$0")/../contrib/release-builder" && pwd)
image=dm-lite-release-builder:$(sha256sum < "$ctx/Dockerfile" | cut -c1-12)-$(id -u)
if ! docker image inspect "$image" >/dev/null 2>&1; then
    docker build -q -t "$image" --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" "$ctx" >/dev/null
fi

echo "build-release: $(git -C "$root" rev-parse HEAD) in $image -> $out${CARGO_BUILD_JOBS:+ (jobs and CPUs: $CARGO_BUILD_JOBS)}"
docker run --rm "${limit[@]}" \
    -v "$root:/dm-lite:ro" -v "$lib_dir:/zvec:ro" -v "$out:/target" \
    -v dm-lite-cargo-registry:/cargo/registry -v dm-lite-cargo-git:/cargo/git \
    "$image" bash -c '
        set -euo pipefail
        export CARGO_TARGET_DIR=/target ZVEC_LIB_DIR=/zvec ZVEC_AUTO_BUILD=0 CARGO_INCREMENTAL=0
        export CFLAGS="-ffile-prefix-map=$PWD=/dm-lite -ffile-prefix-map=$CARGO_HOME=/cargo"
        export CXXFLAGS="$CFLAGS"
        export RUSTFLAGS="--remap-path-prefix=$PWD=/dm-lite --remap-path-prefix=$CARGO_HOME=/cargo --remap-path-prefix=$(rustc --print sysroot)=/rust-sysroot"
        rustc -V; ldd --version | sed -n 1p; echo "jobs: ${CARGO_BUILD_JOBS:-all CPUs}"
        cargo build --release --locked --features dist
    '
echo "build-release: $out/release/dmem sha256 $(sha256sum "$out/release/dmem" | cut -c1-64)"
