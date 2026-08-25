#!/usr/bin/env bash
# ci-run.sh — run one `cargo xtask` subcommand on the CI/build host, capturing
# its full output to a timestamped log outside the repo.
#
# Usage:
#   tools/ci/ci-run.sh [xtask-subcommand and args...]
#
# With no arguments it runs `ci` — the full per-PR gate: fmt,
# clippy, deps-check, cfg-check, test, docs-check, deny, supply-chain,
# fuzz --quick, proptest --quick, model-check, spec-review, abi-check.
#
# Examples:
#   tools/ci/ci-run.sh                                   # full gate
#   tools/ci/ci-run.sh build --headless --target x86_64-unknown-none
#   tools/ci/ci-run.sh test --qemu
#
# `cargo xtask` is the single source of truth for what each check does: this
# wrapper only adds environment setup and logging, never
# new pipeline steps. The process exit status mirrors the xtask exit status so
# cron/launchd and CI dashboards see real pass/fail.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

ci_prepare

# Default to the full gate when invoked with no subcommand.
if [ "$#" -eq 0 ]; then
    set -- ci
fi

stamp="$(ci_stamp)"
# Build a filesystem-safe label from the subcommand line ("build --headless
# --target x86_64-unknown-none" -> "build_--headless_--target_x86_64...").
label="$(printf '%s' "$*" | tr ' /' '__')"
log="$TAIRIX_CI_LOGDIR/${label}-${stamp}.log"

echo "ci-run: cargo xtask $* -> $log"

status=0
cargo xtask "$@" >"$log" 2>&1 || status=$?

echo "ci-run: exit=$status log=$log"
exit "$status"
