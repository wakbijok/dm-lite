# The release build environment (docs/releasing.md step 3): Ubuntu 24.04, so the binary needs
# glibc 2.39 whatever the host's glibc is. scripts/build-release-container.sh builds and runs it.
FROM ubuntu:24.04@sha256:24737cef17f4e22b1977df7d9a05e77c4e28b7da5b8911f8d8ccda136bf3573c

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential cmake pkg-config perl git curl ca-certificates libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# The build runs as the caller's uid, so the target directory on the host stays theirs.
ARG UID=1000
ARG GID=1000
RUN (getent passwd $UID | cut -d: -f1 | xargs -r userdel -r) \
 && (getent group $GID || groupadd -g $GID builder) \
 && useradd -m -u $UID -g $GID builder \
 && mkdir -p /cargo /rustup /target \
 && chown $UID:$GID /cargo /rustup /target
USER builder

# CARGO_HOME and the checkout mount (/dm-lite) are the names the runbook's path remapping
# writes into the binary, so nothing of the build host can appear in it. rust-src is part of
# the toolchain the releases are built with: with it, std's panic locations are under
# /rust-sysroot, without it under /rustc/<hash>, so leaving it out changes the binary.
ARG RUST_VERSION=1.99.1
ENV CARGO_HOME=/cargo RUSTUP_HOME=/rustup PATH=/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain $RUST_VERSION --component rust-src \
 && rustc -V \
 && mkdir -p /cargo/registry /cargo/git
WORKDIR /dm-lite
