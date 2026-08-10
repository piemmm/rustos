#!/usr/bin/env bash
# soak.sh — run the nightly 24 h soaks (AGENTS.md §19.6 fuzz, §19.7 proptest,
# and the §7 repeated-test soak) with every harness, model, and the test
# matrix running IN PARALLEL, one log per job.
#
# `cargo xtask fuzz --soak` and `cargo xtask proptest --soak` already run
# their registries concurrently in-process (bounded by host parallelism).
# This script still fans the registry out into one process per target so the
# nightly soak gets per-target isolation and one log file per target: a crash
# in one harness leaves the others' logs intact, and every target shares the
# single 24 h wall-clock budget. Both orchestrators expose `--target NAME`
# (and `--list`), and the registries stay the single source of truth — this
# script never hard-codes the target list.
#
# `cargo xtask test --soak` is the §7 counterpart: it repeats the whole test
# matrix for the same 24 h budget so a flake too rare to surface in the
# per-PR single-pass run (where `ci` runs each test once) still gets a full
# night of exposure. It is a single job
# (the matrix is one unit), launched alongside the fuzz/proptest fan-out.
#
# Scheduling priority: the test matrix is the ONLY job with a hard, no-retry
# wall-clock deadline per job (its QEMU verticals fail closed on timeout); the
# fuzz/proptest/fssoak soaks are throughput jobs with no per-pass deadline —
# they merely run more passes the more CPU they get. Every job fans out to all
# cores, so launching them all together oversubscribes the host many-fold; the
# QEMU guests then starve for TCG cycles and time out (only the deadline-bound
# test job can fail this way, while the deadline-free soaks just run slower).
# We therefore run the throughput soaks at a lowered scheduling priority (a
# positive `nice`) so they only ever consume CPU the timed test matrix is not
# using: the kernel hands the guests their cores whenever they are runnable,
# keeping the QEMU runner's "owns the host" assumption — and thus every
# guest's wall-clock deadline — true under the full parallel fan-out. Lowering
# one's own niceness never requires privilege, so this works on any runner.
#
# This `nice` split is a STRUCTURAL measure that keeps the host from
# oversubscribing the timed guests — it is NOT a licence to tolerate a timeout.
# A QEMU vertical's deadline is sized to the actual work with headroom; if a
# guest still misses it, that is a genuine defect (a too-tight budget, a missing
# completion signal, an unsynchronised wait) fixed under `AGENTS.md` §7. It is
# NEVER dismissed as a "machine load" flake and NEVER "resolved" by re-running
# the vertical on its own until it passes — every such timeout has turned out to
# be a real bug the load merely exposed.
#
# Usage:
#   tools/ci/soak.sh [fuzz|proptest|fssoak|test|both|all] [--sequential] \
#                    [--secs N] [--dry-run]
#
#   (no kind)      same as `both`: the §19.6 fuzz and §19.7 proptest soaks
#   fuzz           run only the §19.6 fuzz harnesses
#   proptest       run only the §19.7 proptest models
#   fssoak         run only the docs/src/filesystem/soak.md filesystem soak
#   test           run only the §7 repeated-test soak
#   both           run the fuzz and proptest soaks
#   all            run fuzz, proptest, the filesystem soak, and the test soak
#   --sequential   run jobs one at a time (default is parallel)
#   --secs N       override the per-job budget (for smoke runs; CI uses 24 h)
#   --dry-run      print the planned jobs and exit without running anything
#
# Exit status is non-zero if any job fails — §7/§19.6/§19.7 fail closed.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

kind="both"
sequential=0
dry_run=0
secs=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        fuzz | proptest | fssoak | test) kind="$1" ;;
        both | all) kind="$1" ;;
        --sequential) sequential=1 ;;
        --dry-run) dry_run=1 ;;
        --secs)
            shift
            [ "$#" -gt 0 ] || { echo "soak: --secs requires a number" >&2; exit 2; }
            secs="$1"
            ;;
        *)
            echo "soak: unexpected argument '$1'" >&2
            echo "usage: soak.sh [fuzz|proptest|fssoak|test|both|all] [--sequential] [--secs N] [--dry-run]" >&2
            exit 2
            ;;
    esac
    shift
done

ci_prepare

stamp="$(ci_stamp)"
soak_dir="$TAIRIX_CI_LOGDIR/soak-$stamp"
mkdir -p "$soak_dir"

# Optional per-job budget override, applied to both orchestrators.
budget_args=()
if [ -n "$secs" ]; then
    budget_args=(--secs "$secs")
fi

# Parallel arrays describing each launched job (bash 3.2 has no associative
# arrays / `wait -n`, so we track index-aligned plain arrays).
job_labels=()
job_logs=()
job_pids=()

# Niceness applied to the deadline-free throughput soaks (fuzz/proptest/fssoak)
# so they yield CPU to the hard-deadline QEMU `test` matrix (see the priority
# note in the header). 19 is the lowest scheduling priority, the largest
# possible yield.
throughput_nice=19

# enumerate <xtask-subcommand>: print the first column of `--list`, i.e. the
# `--target` selector for every registered harness/model.
enumerate() {
    cargo xtask "$1" --list | awk 'NF { print $1 }'
}

# launch_raw <niceness|-> <label> <xtask-args...>: start (or, when
# --sequential, run) one `cargo xtask <args...>` job, logging to
# "$soak_dir/<label>.log". A numeric <niceness> runs the job at that lowered
# scheduling priority (via `nice`); `-` runs it at normal priority.
launch_raw() {
    local niceness="$1"
    local label="$2"
    shift 2
    local logf="$soak_dir/$label.log"
    echo "soak: $label -> $logf"
    job_labels+=("$label")
    job_logs+=("$logf")
    if [ "$dry_run" -eq 1 ]; then
        return 0
    fi
    # Prefix `nice -n <niceness>` for the deadline-free throughput soaks so
    # they yield CPU to the hard-deadline QEMU test matrix (see the header).
    local run_prefix=()
    if [ "$niceness" != "-" ]; then
        run_prefix=(nice -n "$niceness")
    fi
    if [ "$sequential" -eq 1 ]; then
        local rc=0
        ${run_prefix[@]+"${run_prefix[@]}"} cargo xtask "$@" >"$logf" 2>&1 || rc=$?
        job_pids+=("done:$rc")
    else
        ${run_prefix[@]+"${run_prefix[@]}"} cargo xtask "$@" >"$logf" 2>&1 &
        job_pids+=("$!")
    fi
}

# launch <label> <xtask-subcommand> <target>: a fuzz/proptest/fssoak soak job
# for one registry target, sharing the per-job budget. These are the
# deadline-free throughput soaks, so they run at the lowered
# `$throughput_nice` priority.
launch() {
    local label="$1" subcmd="$2" target="$3"
    launch_raw "$throughput_nice" "$label" "$subcmd" --soak --target "$target" \
        ${budget_args[@]+"${budget_args[@]}"}
}

if [ "$kind" = "both" ] || [ "$kind" = "all" ] || [ "$kind" = "fuzz" ]; then
    while IFS= read -r t; do
        [ -n "$t" ] && launch "fuzz-$t" fuzz "$t"
    done < <(enumerate fuzz)
fi
if [ "$kind" = "both" ] || [ "$kind" = "all" ] || [ "$kind" = "proptest" ]; then
    while IFS= read -r m; do
        [ -n "$m" ] && launch "proptest-$m" proptest "$m"
    done < <(enumerate proptest)
fi
# The docs/src/filesystem/soak.md filesystem soak: one job per registered
# target (arxfs, ext4, fat32, and the randomized arxfs-random), each
# formatting a ≥1 GiB RAM volume and exercising it for the per-job
# budget. The registry (`cargo xtask fssoak --list`) is the single
# source of truth, so this never hard-codes the filesystem list.
if [ "$kind" = "all" ] || [ "$kind" = "fssoak" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] && launch "fssoak-$f" fssoak "$f"
    done < <(enumerate fssoak)
fi
# The §7 repeated-test soak: one job that repeats the whole test matrix
# (host + the bare-metal QEMU verticals) for the per-job budget. `cargo xtask
# test` owns the repeat loop, so the budget covers the matrix as a unit.
# The test matrix runs at normal priority (`-`): it is the deadline-bound job
# the throughput soaks above yield to.
if [ "$kind" = "all" ] || [ "$kind" = "test" ]; then
    launch_raw - "test" test --qemu --soak ${budget_args[@]+"${budget_args[@]}"}
fi

if [ "$dry_run" -eq 1 ]; then
    echo "soak: dry run — ${#job_labels[@]} job(s) planned under $soak_dir"
    exit 0
fi

# Collect every job's result. For parallel jobs `wait <pid>` yields that job's
# exit status; sequential jobs already carry "done:<rc>".
failed=0
i=0
while [ "$i" -lt "${#job_pids[@]}" ]; do
    pid="${job_pids[$i]}"
    label="${job_labels[$i]}"
    logf="${job_logs[$i]}"
    rc=0
    case "$pid" in
        done:*) rc="${pid#done:}" ;;
        *) wait "$pid" || rc=$? ;;
    esac
    if [ "$rc" -eq 0 ]; then
        echo "soak: PASS $label"
    else
        echo "soak: FAIL $label (rc=$rc, see $logf)"
        failed=$((failed + 1))
    fi
    i=$((i + 1))
done

echo "soak: ${#job_labels[@]} job(s), $failed failed; logs in $soak_dir"
[ "$failed" -eq 0 ] || exit 1
