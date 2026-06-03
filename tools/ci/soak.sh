#!/usr/bin/env bash
# soak.sh — run the nightly 24 h soaks (AGENTS.md §19.6 fuzz, §19.7 proptest,
# and the §7 repeated-test soak) with every harness, model, and the test
# matrix running IN PARALLEL, one log per job.
#
# `cargo xtask fuzz --soak` and `cargo xtask proptest --soak` run their
# registries sequentially, so the full nightly would otherwise take
# (harnesses + models) x 24 h. Both orchestrators expose `--target NAME`
# (and `--list`), so this script fans the registry out into one process per
# target, all sharing the single 24 h wall-clock budget. The registries stay
# the single source of truth — this script never hard-codes the target list.
#
# `cargo xtask test --soak` is the §7 counterpart: it repeats the whole test
# matrix for the same 24 h budget so a flake too rare to surface in the
# per-PR 100x run still gets a full night of exposure. It is a single job
# (the matrix is one unit), launched alongside the fuzz/proptest fan-out.
#
# Usage:
#   tools/ci/soak.sh [fuzz|proptest|fssoak|test|both|all] [--sequential] \
#                    [--secs N] [--dry-run]
#
#   (no kind)      same as `both`: the §19.6 fuzz and §19.7 proptest soaks
#   fuzz           run only the §19.6 fuzz harnesses
#   proptest       run only the §19.7 proptest models
#   fssoak         run only the filesystems.md filesystem soak
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
soak_dir="$RUSTOS_CI_LOGDIR/soak-$stamp"
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

# enumerate <xtask-subcommand>: print the first column of `--list`, i.e. the
# `--target` selector for every registered harness/model.
enumerate() {
    cargo xtask "$1" --list | awk 'NF { print $1 }'
}

# launch_raw <label> <xtask-args...>: start (or, when --sequential, run) one
# `cargo xtask <args...>` job, logging to "$soak_dir/<label>.log".
launch_raw() {
    local label="$1"
    shift
    local logf="$soak_dir/$label.log"
    echo "soak: $label -> $logf"
    job_labels+=("$label")
    job_logs+=("$logf")
    if [ "$dry_run" -eq 1 ]; then
        return 0
    fi
    if [ "$sequential" -eq 1 ]; then
        local rc=0
        cargo xtask "$@" >"$logf" 2>&1 || rc=$?
        job_pids+=("done:$rc")
    else
        cargo xtask "$@" >"$logf" 2>&1 &
        job_pids+=("$!")
    fi
}

# launch <label> <xtask-subcommand> <target>: a fuzz/proptest soak job for one
# registry target, sharing the per-job budget.
launch() {
    local label="$1" subcmd="$2" target="$3"
    launch_raw "$label" "$subcmd" --soak --target "$target" \
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
# The filesystems.md filesystem soak: one job per filesystem (rustfs,
# ext4, fat32), each formatting a ≥1 GiB RAM volume and exercising it for
# the per-job budget. The registry (`cargo xtask fssoak --list`) is the
# single source of truth, so this never hard-codes the filesystem list.
if [ "$kind" = "all" ] || [ "$kind" = "fssoak" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] && launch "fssoak-$f" fssoak "$f"
    done < <(enumerate fssoak)
fi
# The §7 repeated-test soak: one job that repeats the whole test matrix
# (host + the bare-metal QEMU verticals) for the per-job budget. `cargo xtask
# test` owns the repeat loop, so the budget covers the matrix as a unit.
if [ "$kind" = "all" ] || [ "$kind" = "test" ]; then
    launch_raw "test" test --qemu --soak ${budget_args[@]+"${budget_args[@]}"}
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
