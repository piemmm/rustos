#!/usr/bin/env bash
# soak.sh — run the nightly 24 h soaks (AGENTS.md §19.6 fuzz, §19.7 proptest)
# with every harness and model running IN PARALLEL, one log per job.
#
# `cargo xtask fuzz --soak` and `cargo xtask proptest --soak` run their
# registries sequentially, so the full nightly would otherwise take
# (harnesses + models) x 24 h. Both orchestrators expose `--target NAME`
# (and `--list`), so this script fans the registry out into one process per
# target, all sharing the single 24 h wall-clock budget. The registries stay
# the single source of truth — this script never hard-codes the target list.
#
# Usage:
#   tools/ci/soak.sh [fuzz|proptest] [--sequential] [--secs N] [--dry-run]
#
#   (no kind)      run both the fuzz and proptest soaks
#   fuzz           run only the §19.6 fuzz harnesses
#   proptest       run only the §19.7 proptest models
#   --sequential   run jobs one at a time (default is parallel)
#   --secs N       override the per-job budget (for smoke runs; CI uses 24 h)
#   --dry-run      print the planned jobs and exit without running anything
#
# Exit status is non-zero if any job fails — §19.6/§19.7 fail closed.
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
        fuzz | proptest) kind="$1" ;;
        both) kind="both" ;;
        --sequential) sequential=1 ;;
        --dry-run) dry_run=1 ;;
        --secs)
            shift
            [ "$#" -gt 0 ] || { echo "soak: --secs requires a number" >&2; exit 2; }
            secs="$1"
            ;;
        *)
            echo "soak: unexpected argument '$1'" >&2
            echo "usage: soak.sh [fuzz|proptest] [--sequential] [--secs N] [--dry-run]" >&2
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

# launch <label> <xtask-subcommand> <target>: start (or, when --sequential,
# run) one soak job, logging to "$soak_dir/<label>.log".
launch() {
    local label="$1" subcmd="$2" target="$3"
    local logf="$soak_dir/$label.log"
    echo "soak: $label -> $logf"
    job_labels+=("$label")
    job_logs+=("$logf")
    if [ "$dry_run" -eq 1 ]; then
        return 0
    fi
    if [ "$sequential" -eq 1 ]; then
        local rc=0
        cargo xtask "$subcmd" --soak --target "$target" \
            ${budget_args[@]+"${budget_args[@]}"} >"$logf" 2>&1 || rc=$?
        job_pids+=("done:$rc")
    else
        cargo xtask "$subcmd" --soak --target "$target" \
            ${budget_args[@]+"${budget_args[@]}"} >"$logf" 2>&1 &
        job_pids+=("$!")
    fi
}

if [ "$kind" = "both" ] || [ "$kind" = "fuzz" ]; then
    while IFS= read -r t; do
        [ -n "$t" ] && launch "fuzz-$t" fuzz "$t"
    done < <(enumerate fuzz)
fi
if [ "$kind" = "both" ] || [ "$kind" = "proptest" ]; then
    while IFS= read -r m; do
        [ -n "$m" ] && launch "proptest-$m" proptest "$m"
    done < <(enumerate proptest)
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
