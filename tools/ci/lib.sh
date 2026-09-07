# shellcheck shell=bash
# lib.sh — shared helpers for the TAIRiX CI/build-host scripts.
#
# This file is *sourced*, never executed. It centralises the three things
# every CI-host script needs (no duplication): the pinned
# toolchain on PATH, the repository root, and a log directory that lives
# OUTSIDE the source tree (no file may exist outside the
# defined layout).
#
# Every value is overridable from the environment so the same scripts drive a
# laptop, a dedicated builder, or a containerised runner without edits.

# Repository root, resolved from this file's own location so the scripts work
# regardless of the caller's working directory (cron and launchd start with an
# unpredictable CWD).
TAIRIX_CI_REPO="${TAIRIX_CI_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

# Where run logs land. Deliberately outside the repo: CI artefacts must not
# land in the tracked tree.
TAIRIX_CI_LOGDIR="${TAIRIX_CI_LOGDIR:-$HOME/ci-logs/tairix}"

# The branch a dedicated CI host tracks. Only consulted when TAIRIX_CI_SYNC=1.
TAIRIX_CI_BRANCH="${TAIRIX_CI_BRANCH:-master}"

# ci_prepare: make the environment safe for an unattended `cargo xtask` run.
#
# - Puts the pinned toolchain on PATH. cron, launchd, and systemd all start
#   jobs with a bare PATH (/usr/bin:/bin:...), but rustup/cargo/rustc live in
#   the cargo bin directory; without this every job fails with "command not
#   found". The location is CARGO_HOME/bin when CARGO_HOME is set (common on
#   Linux CI images), falling back to ~/.cargo/bin (the rustup default on both
#   Linux and macOS). If cargo is already on PATH (a system-wide install) we
#   leave PATH untouched.
# - Forces plain-text cargo output so logs paste back cleanly.
# - Optionally fast-forwards the checkout to the branch tip (TAIRIX_CI_SYNC=1).
#   Off by default so the scripts never destroy uncommitted local work by
#   surprise; a dedicated builder sets TAIRIX_CI_SYNC=1.
ci_prepare() {
    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
    case ":$PATH:" in
        *":$cargo_bin:"*) ;;
        *) PATH="$cargo_bin:$PATH" ;;
    esac
    export PATH
    export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

    if ! command -v cargo >/dev/null 2>&1; then
        echo "ci: cargo not found on PATH (looked in '$cargo_bin'; set CARGO_HOME or install the pinned toolchain)" >&2
        return 127
    fi

    mkdir -p "$TAIRIX_CI_LOGDIR"
    cd "$TAIRIX_CI_REPO" || return 1

    if [ "${TAIRIX_CI_SYNC:-0}" = "1" ]; then
        git fetch --quiet origin "$TAIRIX_CI_BRANCH"
        git reset --hard "origin/$TAIRIX_CI_BRANCH"
    fi
}

# ci_stamp: a filesystem-safe UTC timestamp for log file names.
ci_stamp() {
    date -u +%Y%m%dT%H%M%SZ
}

# ci_collect_qemu_artefacts <destination-dir>: gather the QEMU verticals'
# post-mortems so a failure can still be diagnosed after the run.
#
# `cargo xtask test --qemu` writes each guest's full serial log and, when a
# guest hangs, a dump of every core's CPU state, and names both in its failure
# message. They land under `CARGO_TARGET_DIR`, which is a build cache the
# workflows do not upload and `cargo xtask clean` wipes at the start of the
# next run — so the one artefact a hang is diagnosable from is discarded
# before anyone reads the failure. Copying them beside the logs that *are*
# uploaded is what makes the next hang answerable instead of merely observed.
#
# Never fails the caller: a missing target tree or an unreadable file leaves
# the diagnosis thinner, and losing a whole run over it would be worse.
ci_collect_qemu_artefacts() {
    local dest="$1"
    local target="${CARGO_TARGET_DIR:-$TAIRIX_CI_REPO/target}"
    [ -d "$target" ] || return 0
    mkdir -p "$dest" 2>/dev/null || return 0
    find "$target" -type f \( -name '*.serial.log' -o -name '*.hang.txt' \) \
        -exec cp -p {} "$dest/" \; 2>/dev/null
    return 0
}
