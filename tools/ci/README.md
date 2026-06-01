# `tools/ci` — CI / build-host orchestration

Scripts for standing up an unattended RustOS build/CI/test machine. They are
thin wrappers around `cargo xtask`, which is the **single source of truth** for
what every check does (`AGENTS.md` §15): these scripts add only environment
setup, logging, and parallel scheduling — never new pipeline steps. A new check
belongs in a *named* `cargo xtask` subcommand (`tools/xtask`), not here.

## Files

| File | Purpose |
|------|---------|
| `lib.sh` | Sourced by the others. Puts the pinned toolchain on `PATH`, resolves the repo root, sets the log directory, and (opt-in) syncs the checkout. |
| `ci-run.sh` | Run one `cargo xtask` subcommand, logging to a timestamped file. Default subcommand is `ci` (the full per-PR gate, §7). |
| `soak.sh` | Run the nightly 24 h soaks (§19.6 fuzz, §19.7 proptest) with every harness/model **in parallel**, one log per job. |
| `crontab.sample` | Ready-to-edit `crontab` for a Linux/Unix builder. |
| `launchd/*.plist.sample` | `launchd` LaunchAgents for a macOS host (preferred over cron on laptops). |

## Quick start

```sh
# Full per-PR gate once, logged:
tools/ci/ci-run.sh                       # == cargo xtask ci

# A single targeted check:
tools/ci/ci-run.sh build --headless --target x86_64-unknown-none
tools/ci/ci-run.sh test --qemu

# The nightly soaks, all harnesses + models in parallel:
tools/ci/soak.sh

# See exactly which soak jobs would run, without waiting 24 h:
tools/ci/soak.sh --dry-run
```

## Scheduling

- **Linux/Unix:** `crontab tools/ci/crontab.sample` (edit `REPO` first).
- **macOS:** copy the plists into `~/Library/LaunchAgents/` and
  `launchctl load` them. `launchd` survives sleep/wake more predictably than
  cron on a laptop. The script bodies are identical; only the trigger differs.

Both run the full gate every 30 minutes and the soaks nightly at 02:00.

## The 24 h soaks, in parallel

`cargo xtask fuzz --soak` and `cargo xtask proptest --soak` each run their
registries **sequentially**, 24 h per target — so the full nightly would take
`(harnesses + models) x 24 h`. `soak.sh` reads each registry via
`cargo xtask <fuzz|proptest> --list` (so the target list is never duplicated
here) and launches one `--soak --target <name>` process per target, all sharing
a single 24 h wall clock. Each job writes its own log under
`<logdir>/soak-<UTC-stamp>/<job>.log`, and the script exits non-zero if any job
fails — §19.6/§19.7 fail closed.

```sh
tools/ci/soak.sh                 # both, parallel (default)
tools/ci/soak.sh fuzz            # only the §19.6 fuzz harnesses
tools/ci/soak.sh proptest        # only the §19.7 proptest models
tools/ci/soak.sh --sequential    # one at a time (low-resource hosts)
tools/ci/soak.sh --secs 30       # short budget for a smoke run
```

## Configuration (environment variables)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RUSTOS_CI_REPO` | resolved from the script path | Repository root. |
| `RUSTOS_CI_LOGDIR` | `~/ci-logs/rustos` | Where logs land — **outside** the repo (`AGENTS.md` §3: no CI artefact in the tracked tree). |
| `RUSTOS_CI_BRANCH` | `master` | Branch a dedicated builder tracks. |
| `RUSTOS_CI_SYNC` | `0` | `1` = `git fetch && git reset --hard origin/<branch>` before each run. Off by default so a developer checkout is never reset by surprise; set it on a dedicated builder. |

## Notes

- **Toolchain on `PATH`.** cron/launchd start with a bare `PATH`; the pinned
  toolchain lives in `~/.cargo/bin`. `lib.sh` prepends it, so jobs do not fail
  with "command not found". The toolchain version itself is pinned by
  `rust-toolchain.toml`, not by these scripts.
- **Handing logs back.** One file per run; `cargo xtask` runs steps in order
  and fails closed, so the failing step is at the **tail** of its log. For a
  soak crash, the reproducer in the job log is what turns into a regression
  test + corpus entry (§19.6).
- **Self-hosting honesty.** `AGENTS.md` §19.3 forbids the *OS* from fetching
  executable code post-install; that governs RustOS, not the build host. A
  normal `cargo`/`rustup` host is fine. Keep `Cargo.lock` committed so
  `supply-chain` source-hash pinning stays meaningful.
