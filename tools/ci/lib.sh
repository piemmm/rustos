# shellcheck shell=bash
# lib.sh — shared helpers for the RustOS CI/build-host scripts.
#
# This file is *sourced*, never executed. It centralises the three things
# every CI-host script needs (AGENTS.md §2.2 — no duplication): the pinned
# toolchain on PATH, the repository root, and a log directory that lives
# OUTSIDE the source tree (AGENTS.md §3 — no file may exist outside the
# defined layout).
#
# Every value is overridable from the environment so the same scripts drive a
# laptop, a dedicated builder, or a containerised runner without edits.

# Repository root, resolved from this file's own location so the scripts work
# regardless of the caller's working directory (cron and launchd start with an
# unpredictable CWD).
RUSTOS_CI_REPO="${RUSTOS_CI_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

# Where run logs land. Deliberately outside the repo: CI artefacts must not
# land in the tracked tree (AGENTS.md §3).
RUSTOS_CI_LOGDIR="${RUSTOS_CI_LOGDIR:-$HOME/ci-logs/rustos}"

# The branch a dedicated CI host tracks. Only consulted when RUSTOS_CI_SYNC=1.
RUSTOS_CI_BRANCH="${RUSTOS_CI_BRANCH:-master}"

# ci_prepare: make the environment safe for an unattended `cargo xtask` run.
#
# - Puts the pinned toolchain on PATH. cron/launchd start with a bare PATH
#   (/usr/bin:/bin:...), but rustup/cargo/rustc live in ~/.cargo/bin on this
#   host; without this every job fails with "command not found".
# - Forces plain-text cargo output so logs paste back cleanly.
# - Optionally fast-forwards the checkout to the branch tip (RUSTOS_CI_SYNC=1).
#   Off by default so the scripts never destroy uncommitted local work by
#   surprise; a dedicated builder sets RUSTOS_CI_SYNC=1.
ci_prepare() {
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    export PATH
    export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

    if ! command -v cargo >/dev/null 2>&1; then
        echo "ci: cargo not found on PATH (expected the pinned toolchain in ~/.cargo/bin)" >&2
        return 127
    fi

    mkdir -p "$RUSTOS_CI_LOGDIR"
    cd "$RUSTOS_CI_REPO" || return 1

    if [ "${RUSTOS_CI_SYNC:-0}" = "1" ]; then
        git fetch --quiet origin "$RUSTOS_CI_BRANCH"
        git reset --hard "origin/$RUSTOS_CI_BRANCH"
    fi
}

# ci_stamp: a filesystem-safe UTC timestamp for log file names.
ci_stamp() {
    date -u +%Y%m%dT%H%M%SZ
}
