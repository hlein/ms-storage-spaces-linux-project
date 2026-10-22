#!/bin/bash
# Builds the static release binaries (x86_64-unknown-linux-musl: spaces,
# spaces-pattern, refs) as the releases ship them, into target/ (or
# CARGO_TARGET_DIR): the paths of the source tree, of the crates, of the
# target directory and of the toolchain's sources (which the standard
# library's code compiled into them names, under ~/.rustup when rust-src is
# installed; mapped back to /rustc/COMMIT, as without it) remapped, so that
# the binaries do not name the machine they were built on and two builds of
# one tree in different directories are the same (tools/repro-check.sh).
# The release profile strips symbols.
# Usage: tools/build-static.sh [CARGO-BUILD-ARGS...]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo_home=${CARGO_HOME:-$HOME/.cargo}
target=${CARGO_TARGET_DIR:-$PWD/target}
rust_src=$(rustc --print sysroot)/lib/rustlib/src/rust
rust_commit=$(rustc -vV | sed -n 's/^commit-hash: //p')
flags="--remap-path-prefix=$target=/target --remap-path-prefix=$PWD=/build --remap-path-prefix=$cargo_home=/cargo"
flags+=" --remap-path-prefix=$rust_src=/rustc/$rust_commit"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags"
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 0)}
export SOURCE_DATE_EPOCH
exec cargo build --locked --release --target x86_64-unknown-linux-musl -p spaces-cli -p refs-cli "$@"
