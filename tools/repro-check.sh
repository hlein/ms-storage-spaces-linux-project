#!/bin/bash
# Builds the static binaries of this working tree twice, from two copies of
# it in directories of different names and lengths (tools/build-static.sh,
# each with its own target directory), and compares them byte for byte.
# Prints the SHA-256 of each binary; exits non-zero when they differ, or
# when a binary names the machine it was built on: the home directory (the
# toolchain's sources under ~/.rustup, the crates under ~/.cargo), the
# toolchain's sysroot or the build directories.
# Usage: tools/repro-check.sh WORKDIR   (made if missing; its a/ and
#        build-two/ are replaced)
set -euo pipefail
cd "$(dirname "$0")/.."
work=${1:?usage: tools/repro-check.sh WORKDIR}
mkdir -p "$work"
for d in a build-two; do
  rm -rf -- "${work:?}/$d"
  mkdir -p "$work/$d"
  rsync -a --exclude /target/ --exclude /testdata --exclude /.git/ --exclude /fuzz/target/ \
    --exclude /fuzz/corpus/ --exclude /fuzz/artifacts/ ./ "$work/$d/"
  (cd "$work/$d" && SOURCE_DATE_EPOCH=0 nice -n 19 tools/build-static.sh -q)
done
rc=0
for bin in spaces spaces-pattern refs; do
  a=$(sha256sum < "$work/a/target/x86_64-unknown-linux-musl/release/$bin" | cut -d' ' -f1)
  b=$(sha256sum < "$work/build-two/target/x86_64-unknown-linux-musl/release/$bin" | cut -d' ' -f1)
  if [[ $a == "$b" ]]; then echo "same $bin $a"; else echo "DIFFERENT $bin $a $b"; rc=1; fi
  for d in a build-two; do
    if n=$(strings -n 4 "$work/$d/target/x86_64-unknown-linux-musl/release/$bin" |
      grep -c -F -e "$HOME" -e "$(rustc --print sysroot)" -e "$work"); then
      echo "NAMES THE MACHINE $bin ($d): $n strings"
      rc=1
    fi
  done
done
exit "$rc"
