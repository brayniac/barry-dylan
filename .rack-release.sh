#!/bin/bash
# What a release means for this repository, run by rack-ci in an ephemeral guest
# when a `v*` tag appears: build the Debian package and leave it in `dist/`. The
# contract is exactly that -- a .deb in dist/, exit 0 -- and the rest (upload,
# the barrier, publish-apt-internal on delta) is rack-ci's.
#
# One package, unlike anvil's four: barry-dylan is a single binary that is both
# the service on delta and the reviewer inside a rack guest. That second use is
# why this publishes to the internal repo at all -- an ephemeral VM installs
# barry-dylan over plain HTTP from delta, carrying no credential, and gets
# `review-offline` and `judge-offline` from it.
set -uo pipefail

log() { echo "===> $*"; }

# The x86 target is a Debian guest on delta's validation shape, which does not
# carry Rust. Same bootstrap as anvil's and slipway's release scripts.
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    log "no cargo on this image -- installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal || {
        log "rustup install failed"
        exit 1
    }
    . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 || { log "still no cargo after install"; exit 1; }
log "cargo: $(cargo --version)"

command -v cargo-deb >/dev/null 2>&1 || {
    log "installing cargo-deb"
    cargo install cargo-deb --locked || exit 1
}

rm -rf dist && mkdir -p dist

# Built once here and packaged with --no-build, so cargo-deb does not re-check
# the tree it was just handed.
log "cargo build --release"
cargo build --release || { log "build FAILED"; exit 1; }

log "cargo deb"
cargo deb --no-build --output dist/ || { log "package FAILED"; exit 1; }

shopt -s nullglob
debs=(dist/*.deb)
if [ ${#debs[@]} -ne 1 ]; then
    log "expected 1 package in dist/, found ${#debs[@]}"
    exit 1
fi
log "built $(basename "${debs[0]}")"
