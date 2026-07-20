# `tools/ci` — CI / build-host orchestration

Scripts for standing up an unattended TAIRiX build/CI/test machine. They are
thin wrappers around `cargo xtask`, which is the **single source of truth** for
what every check does (`AGENTS.md` §15): these scripts add only environment
setup, logging, and parallel scheduling — never new pipeline steps. A new check
belongs in a *named* `cargo xtask` subcommand (`tools/xtask`), not here.

## Files

| File | Purpose |
|------|---------|
| `lib.sh` | Sourced by the others. Puts the pinned toolchain on `PATH`, resolves the repo root, sets the log directory, and (opt-in) syncs the checkout. |
| `ci-run.sh` | Run one `cargo xtask` subcommand, logging to a timestamped file. Default subcommand is `ci` (the full per-PR gate, §7). |
| `soak.sh` | Run the nightly 24 h soaks (§19.6 fuzz, §19.7 proptest, and the §7 repeated-test soak) with every harness/model/the test matrix **in parallel**, one log per job. |
| `crontab.sample` | Ready-to-edit `crontab` for any cron-based host (Linux/Unix/macOS). |
| `systemd/*.{service,timer}` | systemd user units for a Linux host (preferred over cron on systemd distros). |
| `launchd/*.plist.sample` | `launchd` LaunchAgents for a macOS host (preferred over cron on laptops). |
| `github-runner/README.md` | Standing up a self-hosted GitHub Actions runner (Linux) for the nightly soak workflow. |

The `lib.sh`/`ci-run.sh`/`soak.sh` scripts themselves are portable `bash`
(written to the host's bash 3.2, so they also run unchanged on the bash 4/5
shipped by Linux distros) and use only POSIX utilities (`date -u`, `awk`,
`tr`, `git`, `cargo`). They run identically on Linux and macOS; only the
*scheduler* differs, which is why there is a sample per platform.

## Quick start

```sh
# Full per-PR gate once, logged:
tools/ci/ci-run.sh                       # == cargo xtask ci

# A single targeted check:
tools/ci/ci-run.sh build --headless --target x86_64-unknown-none
tools/ci/ci-run.sh test --qemu

# The nightly soaks: every harness, model, and the test matrix in parallel:
tools/ci/soak.sh all

# See exactly which soak jobs would run, without waiting 24 h:
tools/ci/soak.sh --dry-run
```

## Scheduling

Pick the scheduler your host already runs; the script bodies are identical, so
only the trigger differs.

- **Linux (systemd) — preferred:** install the user units and enable the
  timers (no root needed):
  ```sh
  mkdir -p ~/.config/systemd/user
  cp tools/ci/systemd/tairix-*.{service,timer} ~/.config/systemd/user/
  # edit the absolute ExecStart path in each .service to match your checkout
  systemctl --user daemon-reload
  systemctl --user enable --now tairix-ci.timer tairix-soak.timer
  loginctl enable-linger "$USER"   # let timers fire while logged out
  ```
- **Linux/Unix (cron):** `crontab tools/ci/crontab.sample` (edit `REPO` first).
- **macOS (launchd):** copy the plists into `~/Library/LaunchAgents/` and
  `launchctl load` them. `launchd` survives sleep/wake more predictably than
  cron on a laptop.

All three run the full gate every 30 minutes and the soaks nightly at 02:00.
The systemd timers and the cron `Persistent` behaviour both catch up a run
missed while the host was asleep.

### GitHub Actions

The repo also ships GitHub Actions workflows so CI is driven by GitHub directly,
not only by a standalone builder:

- `.github/workflows/ci.yml` runs the full per-PR gate (`cargo xtask ci`) on a
  **GitHub-hosted** `ubuntu-latest` runner — free and ephemeral.
- `.github/workflows/soak.yml` runs the nightly 24 h soaks (`soak.sh all`:
  fuzz, proptest, and the repeated-test soak) on a **self-hosted Linux**
  runner, because a 24 h job exceeds the GitHub-hosted per-job time cap.

See `tools/ci/github-runner/README.md` to register and install the self-hosted
Linux runner as a systemd service. A given host runs *either* a standalone
builder (cron/systemd/launchd above) *or* a GitHub Actions runner — not both.

## The 24 h soaks, in parallel

`cargo xtask fuzz --soak` and `cargo xtask proptest --soak` each run their
registries **sequentially**, 24 h per target — so the full nightly would take
`(harnesses + models) x 24 h`. `soak.sh` reads each registry via
`cargo xtask <fuzz|proptest> --list` (so the target list is never duplicated
here) and launches one `--soak --target <name>` process per target, all sharing
a single 24 h wall clock. The §7 repeated-test soak adds one more job,
`cargo xtask test --qemu --soak`, that repeats the whole test matrix for the
same budget. Its purpose is **detection, not tolerance**: it exists to *catch*
a defect too rare for the per-PR single-pass run (where `ci` runs each test
once), so the bug can be fixed — never to make an intermittent failure
acceptable. Each job writes its own log under
`<logdir>/soak-<UTC-stamp>/<job>.log`, and the script exits non-zero if any job
fails — §7/§19.6/§19.7 fail closed. Any failure it surfaces is a real defect to
diagnose and fix (§7), not a flake to re-run away.

**A QEMU vertical must never time out.** Every QEMU vertical's wall-clock
deadline is sized to the actual work with headroom, so the guest completes well
inside it — not sized to a quiet, idle host. If a guest ever misses its
deadline, that is a **genuine defect** — a budget too tight for the work, a
missing completion signal, an unsynchronised wait, or the host being allowed to
oversubscribe the timed guests — and it is fixed structurally under the charter
(`AGENTS.md` §7: no flaky tests; "machine load" is never an accepted
diagnosis). It is **never** dismissed as a load flake, and **never** "resolved"
by re-running the vertical on its own until it happens to pass. Historically,
every timeout blamed on "machine load" in this project has turned out to be
exactly such a real bug that the load merely exposed.

The soak's job scheduling is one such **structural** measure — it bounds how
badly the parallel fan-out can contend with the timed guests, it is not a
licence to tolerate a timeout. Every job fans out to all cores, so running them
together oversubscribes the host many-fold; the QEMU verticals in the test
matrix are the ones with a hard wall-clock deadline. The fuzz/proptest/fssoak
jobs therefore run at a lowered scheduling priority (`nice`) and the
repeated-test matrix at normal priority, so the kernel hands the timed guests
their cores whenever runnable. If a vertical still cannot make its deadline
under this arrangement, the deadline or the test is wrong and is fixed (§7) —
the scheduling split is not the last word, correctness is.

```sh
tools/ci/soak.sh                 # both fuzz + proptest, parallel (default)
tools/ci/soak.sh all             # fuzz + proptest + the repeated-test soak
tools/ci/soak.sh fuzz            # only the §19.6 fuzz harnesses
tools/ci/soak.sh proptest        # only the §19.7 proptest models
tools/ci/soak.sh test            # only the §7 repeated-test soak
tools/ci/soak.sh --sequential    # one at a time (low-resource hosts)
tools/ci/soak.sh --secs 30       # short budget for a smoke run
```

## Configuration (environment variables)

| Variable | Default | Meaning |
|----------|---------|---------|
| `TAIRIX_CI_REPO` | resolved from the script path | Repository root. |
| `TAIRIX_CI_LOGDIR` | `~/ci-logs/tairix` | Where logs land — **outside** the repo (`AGENTS.md` §3: no CI artefact in the tracked tree). |
| `TAIRIX_CI_BRANCH` | `master` | Branch a dedicated builder tracks. |
| `TAIRIX_CI_SYNC` | `0` | `1` = `git fetch && git reset --hard origin/<branch>` before each run. Off by default so a developer checkout is never reset by surprise; set it on a dedicated builder. |

## Notes

- **Toolchain on `PATH`.** cron, launchd, and systemd all start jobs with a
  bare `PATH`; the pinned toolchain lives in `${CARGO_HOME:-~/.cargo}/bin`
  (the rustup default on both Linux and macOS). `lib.sh` prepends that
  directory — honouring `CARGO_HOME` for Linux CI images that relocate it —
  unless `cargo` is already on `PATH` (a system-wide install), so jobs do not
  fail with "command not found". The toolchain version itself is pinned by
  `rust-toolchain.toml`, not by these scripts.
- **Handing logs back.** One file per run; `cargo xtask` runs steps in order
  and fails closed, so the failing step is at the **tail** of its log. For a
  soak crash, the reproducer in the job log is what turns into a regression
  test + corpus entry (§19.6).
- **Self-hosting honesty.** `AGENTS.md` §19.3 forbids the *OS* from fetching
  executable code post-install; that governs TAIRiX, not the build host. A
  normal `cargo`/`rustup` host is fine. Keep `Cargo.lock` committed so
  `supply-chain` source-hash pinning stays meaningful.
