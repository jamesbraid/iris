#!/bin/bash
# scripts/vpro-goldens.sh: the VPro (Odyssey) regression nets.
#
# Run from the repo root:  scripts/vpro-goldens.sh COMMAND
#
#   check            draw-level frames and vertex numbers, bit-exact (cargo test)
#   tolerance        the same, passing a frame that moved inside its stored limits
#   bless            re-take the frames, hashes and vertex numbers
#   replay           replay the recorded guest sessions to their checkpoints
#                    (fails when there are no recordings)
#   replay-bless     re-take the recordings' checkpoints
#   gltest DIR       check the guest pixtest/glstate output in DIR against the
#                    committed lists of passing cases
#   gltest-bless DIR re-take the lists from DIR
#   perf             raster_bench against the committed baseline; fails on a
#                    drop of more than PERF_DROP percent (default 20)
#   perf-bless       re-take the baseline
#
# VPRO_RUN is a prefix for the build container when cargo is not on the host
# (for example VPRO_RUN="/path/to/icargo"); it is put before `env`, so the
# variables below reach the tests inside it. Paths given to the tests must be
# visible there.
#
# The README beside the data, src/dev/odyssey/testdata/README.md, says when
# to re-take each net and how to judge a change by its tolerance.

set -euo pipefail
cd "$(dirname "$0")/.."

run=${VPRO_RUN:-}
cargo_test() { # ENV... -- FILTER [TEST-HARNESS-ARGS...]
    local envs=()
    while [ "$1" != "--" ]; do envs+=("$1"); shift; done
    shift
    local filter=$1
    shift
    # shellcheck disable=SC2086
    $run env "${envs[@]}" cargo test --release -p iris-gfx --lib "$filter" -- "$@"
}

baseline=crates/iris-gfx/src/dev/odyssey/testdata/perf/raster_bench.tsv

# Run raster_bench; prints "scene best-Mpix/s" for each scene, the best of
# PERF_RUNS runs (default 3), since a loaded host only ever slows a pass down.
bench() {
    local only=${1:-}
    local n=${PERF_RUNS:-3}
    for _ in $(seq "$n"); do
        # shellcheck disable=SC2086
        $run env BENCH_SECS=1 ${only:+BENCH_SCENE=$only} cargo test --release -p iris-gfx --lib odyssey::raster_bench -- --ignored --nocapture --test-threads=1 2>&1 < /dev/null
    done | awk 'match($0, /bench [A-Za-z0-9]+ +[0-9.]+ Mpix\/s best/) {
                    # The first line follows the harness'"'"'s "test ... " on the same line.
                    split(substr($0, RSTART, RLENGTH), f, " ")
                    if (!(f[2] in best) || f[3] + 0 > best[f[2]] + 0) best[f[2]] = f[3]
                    if (!(f[2] in seen)) { seen[f[2]] = 1; order[++k] = f[2] }
                }
                END { for (i = 1; i <= k; i++) printf "%s %s\n", order[i], best[order[i]] }'
}

cmd=${1:-check}
case $cmd in
check)
    cargo_test -- odyssey::golden
    ;;
tolerance)
    cargo_test VPRO_GOLDEN=tolerance -- odyssey::golden
    ;;
bless)
    cargo_test VPRO_GOLDEN_BLESS=1 -- odyssey::golden::bless --ignored --nocapture
    ;;
replay)
    cargo_test VPRO_REPLAY_REQUIRE=1 -- odyssey::golden::replay::recorded_sessions --nocapture
    ;;
replay-bless)
    cargo_test VPRO_REPLAY_BLESS=1 -- odyssey::golden::replay::replay_bless --ignored --nocapture
    ;;
gltest)
    dir=${2:?usage: vpro-goldens.sh gltest DIR}
    cargo_test VPRO_GLTEST_REQUIRE=1 "VPRO_GLTEST_RESULTS=$dir" -- odyssey::golden::cases --nocapture
    ;;
gltest-bless)
    dir=${2:?usage: vpro-goldens.sh gltest-bless DIR}
    cargo_test VPRO_GLTEST_BLESS=1 "VPRO_GLTEST_RESULTS=$dir" -- odyssey::golden::cases::gltest_bless --ignored --nocapture
    ;;
perf)
    [ -f "$baseline" ] || { echo "no baseline $baseline" >&2; exit 2; }
    drop=${PERF_DROP:-20}
    now=$(bench)
    status=0
    while read -r scene base; do
        case $scene in '#'* | '') continue ;; esac
        cur=$(echo "$now" | awk -v s="$scene" '$1 == s { print $2 }')
        if [ -z "$cur" ]; then
            echo "MISSING $scene (baseline $base Mpix/s)"
            status=1
            continue
        fi
        # A scene under the threshold gets two more tries before it is called slow.
        if awk -v c="$cur" -v b="$base" -v d="$drop" 'BEGIN { exit !(c < b * (100 - d) / 100) }'; then
            cur=$(PERF_RUNS=2 bench "$scene" | awk -v c="$cur" '{ print ($2 > c ? $2 : c) }')
        fi
        if awk -v c="$cur" -v b="$base" -v d="$drop" 'BEGIN { exit !(c < b * (100 - d) / 100) }'; then
            awk -v s="$scene" -v c="$cur" -v b="$base" 'BEGIN { printf "SLOW    %-18s %8.2f Mpix/s, baseline %8.2f (%.0f%%)\n", s, c, b, 100 * c / b }'
            status=1
        else
            awk -v s="$scene" -v c="$cur" -v b="$base" 'BEGIN { printf "ok      %-18s %8.2f Mpix/s, baseline %8.2f (%.0f%%)\n", s, c, b, 100 * c / b }'
        fi
    done < "$baseline"
    exit $status
    ;;
perf-bless)
    {
        echo "# raster_bench best Mpix/s a scene, the best of ${PERF_RUNS:-3} runs, from $(git rev-parse --short HEAD) on $(grep -m1 'model name' /proc/cpuinfo | sed 's/.*: //')."
        echo "# Take it and check it with the same build profile (the fast profile, thin LTO); numbers from another host or profile do not compare."
        bench
    } > "$baseline"
    cat "$baseline"
    ;;
*)
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
