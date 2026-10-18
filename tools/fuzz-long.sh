#!/bin/bash
# The long fuzzing run of the release checklist: every cargo-fuzz target for
# SECONDS (default 24 h), one worker each at nice 19 and at most 2 GiB of
# memory each, from and into fuzz/corpus/<target> (with the seeds
# fuzz/seeds.py writes there, and fuzz/dicts/<target>.dict where there is
# one). OUT (default
# testdata/work/fuzz-long) gets a log per target, the commit, the start and
# end times and each target's exit status in `results`; findings land in
# fuzz/artifacts/<target>. A run cut short (a host crash) is resumed by
# running again with the remaining seconds: the corpus keeps what it found.
# Usage: tools/fuzz-long.sh [SECONDS] [OUT] [TARGET...]
set -uo pipefail
cd "$(dirname "$0")/.." || exit
seconds=${1:-86400}
out=${2:-testdata/work/fuzz-long}
shift $(($# < 2 ? $# : 2))
targets=("$@")
((${#targets[@]})) || mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' fuzz/Cargo.toml | grep -v '^storage-spaces-fuzz$')
mkdir -p "$out"
# Absolute: the runs write it from fuzz/ (a path given absolute broke that).
out=$(cd "$out" && pwd)
git rev-parse --short HEAD > "$out/commit"
git status --porcelain --untracked-files=no | grep -q . && echo "(with uncommitted changes)" >> "$out/commit"
for t in "${targets[@]}"; do
  (cd fuzz && nice -n 19 cargo +nightly fuzz build "$t" >/dev/null 2>&1) || { echo "cannot build $t" >&2; exit 1; }
done
python3 fuzz/seeds.py || { echo "cannot write the seeds" >&2; exit 1; }
date -Iseconds > "$out/started"
: > "$out/results"
for t in "${targets[@]}"; do
  mkdir -p "fuzz/corpus/$t" "fuzz/artifacts/$t"
  dict=()
  [ -f "fuzz/dicts/$t.dict" ] && dict=(-dict="dicts/$t.dict")
  (
    cd fuzz &&
      nice -n 19 "target/x86_64-unknown-linux-gnu/release/$t" -artifact_prefix="artifacts/$t/" \
        -max_total_time="$seconds" -rss_limit_mb=2048 -timeout=300 -print_final_stats=1 "${dict[@]}" "corpus/$t" \
        > "$out/$t.log" 2>&1
    echo "$t exit $?" >> "$out/results"
  ) &
done
wait
date -Iseconds > "$out/finished"
cat "$out/results"
# A target that did not run its time out (a crash, a leak, a timeout: its
# finding is in fuzz/artifacts/<target>) fails the run (1.2.2's script
# exited 0 whatever the targets did).
bad=$(grep -vc ' exit 0$' "$out/results" || true)
missing=$((${#targets[@]} - $(grep -c ' exit ' "$out/results" || true)))
if ((bad > 0 || missing > 0)); then
  echo "$bad target(s) failed, $missing gave no status: see $out and fuzz/artifacts" >&2
  exit 1
fi
