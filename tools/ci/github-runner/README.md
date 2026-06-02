# `tools/ci/github-runner` — self-hosted GitHub Actions runner (Linux)

This directory documents standing up a **self-hosted GitHub Actions runner on
Linux** for RustOS. It is the GitHub-native counterpart of the
cron/systemd/launchd builders in the parent `tools/ci/` directory: same
`cargo xtask` pipeline, just triggered by GitHub instead of a local timer.

## Why a self-hosted runner is needed

There are two CI workloads, and they want different runners:

| Workload | Workflow | Runner | Why |
|----------|----------|--------|-----|
| Per-push gate (`cargo xtask ci`) | `.github/workflows/ci.yml` | self-hosted `[self-hosted, linux]` | Runs the full `cargo xtask ci` pipeline plus the 20× test repeat and the `>= 120 s` per-PR soak gate on a machine we own. |
| Continuous 12 h soak blocks (§19.6 fuzz, §19.7 proptest, §7 repeated tests) | `.github/workflows/soak.yml` | self-hosted `[self-hosted, linux, soak]` | A 12 h block exceeds the GitHub-hosted per-job time cap; it must run on a machine we own, and on its own label so it never blocks `ci`. |

`soak.yml` runs `tools/ci/soak.sh all`, which fans every fuzz harness, proptest
model, and the §7 repeated-test matrix out into parallel `--soak` processes
sharing one 12 h wall clock, then re-dispatches itself so blocks chain back to
back. That is the same script a standalone cron/systemd/launchd builder runs,
so this runner and a standalone builder produce equivalent results.

## Labels and concurrent runs on one host

`ci.yml` targets `runs-on: [self-hosted, linux]`; `soak.yml` targets
`runs-on: [self-hosted, linux, soak]`. `self-hosted` and `linux` are applied
automatically by `config.sh` on a Linux host; the extra `soak` label is what
keeps the two workloads apart.

A single runner instance executes **one job at a time**, so a 12 h soak block
running on the same instance as the per-push gate would queue every `ci` run
behind it for half a day. To let `ci` and `soak` run **concurrently and
independently on the same host**, register **two runner instances** on that
host — one without the `soak` label for `ci`, one with it for `soak`:

| Runner instance | Labels | Picks up |
|-----------------|--------|----------|
| `<host>-rustos-ci`   | `self-hosted,linux`      | `ci.yml` (and, when idle, could also serve a `soak` job — but `soak` requires the `soak` label, so it never lands here). |
| `<host>-rustos-soak` | `self-hosted,linux,soak` | `soak.yml` only — `soak`'s `runs-on` requires the `soak` label, so the long block never occupies the `ci` runner. |

Because `soak`'s `runs-on` demands the `soak` label, soak jobs only ever land
on the `soak` instance; the `ci` instance stays free for per-push gates. The
two instances have separate work directories, so a 12 h soak and a `ci` run
execute side by side without contending for the same checkout or `target/`.
Using a single box for both is fine; if you instead dedicate one machine to
soaks, just register the `soak`-labelled instance there.

## Host prerequisites

- A Linux x86_64 or aarch64 host you control (a VM or spare box is fine).
- `rustup` installed **for the user the runner runs as**, so the pinned
  toolchain in `rust-toolchain.toml` resolves. The workflow runs
  `rustup toolchain install` / `rustup component add` on first use, but
  `rustup` itself must already be on that user's `PATH`
  (`${CARGO_HOME:-~/.cargo}/bin`; see the toolchain note in the parent
  `tools/ci/README.md`).
- Outbound HTTPS to GitHub (to pull jobs). Per AGENTS.md §19.3 this governs the
  build host, not RustOS itself; keep `Cargo.lock` committed so source-hash
  pinning stays meaningful.

## Register the runner

The runner binary is downloaded from the repository's own
**Settings → Actions → Runners → New self-hosted runner** page, which shows the
current download URL and a short-lived registration token. Follow that page;
the steps below are the stable shape of it.

```sh
# As the dedicated runner user, in a directory it owns:
mkdir -p ~/actions-runner && cd ~/actions-runner

# Download + extract the runner (use the exact URL/version from the
# "New self-hosted runner" page — do not hard-code a stale version here).
curl -o actions-runner.tar.gz -L "<url-from-the-runners-page>"
tar xzf actions-runner.tar.gz

# Register against the repository with the short-lived token from that page.
./config.sh \
  --url https://github.com/<owner>/rustos \
  --token <registration-token> \
  --name "$(hostname)-rustos-soak" \
  --labels self-hosted,linux,soak \
  --unattended --replace
```

To also serve `ci.yml` on the same host (so the two run concurrently — see
*Labels and concurrent runs on one host* above), register a **second** instance
in its own directory **without** the `soak` label:

```sh
mkdir -p ~/actions-runner-ci && cd ~/actions-runner-ci
# Download + extract the runner the same way (exact URL from the runners page).
./config.sh \
  --url https://github.com/<owner>/rustos \
  --token <registration-token> \
  --name "$(hostname)-rustos-ci" \
  --labels self-hosted,linux \
  --unattended --replace
```

Install and start each instance as its own service (the `svc.sh` step below);
the two services run side by side.

## Run it as a service (survives logout and reboot)

The runner ships its own systemd integration; use it rather than hand-writing a
unit:

```sh
sudo ./svc.sh install "$USER"   # generate + enable a systemd service for this user
sudo ./svc.sh start             # start it now
sudo ./svc.sh status            # verify it is listening for jobs
```

`svc.sh install` creates a systemd service that auto-restarts and starts on
boot, so the host is ready for the 02:00 UTC `soak.yml` trigger without anyone
logged in. Run it from each instance's directory so both the `ci` and `soak`
runner services come up on boot and run concurrently. (This is separate from
the `tools/ci/systemd/` units, which drive a *standalone* builder; you want the
GitHub runner(s) or the standalone builder, not both, on a given host.)

## Verify

- The runner appears **Idle** under Settings → Actions → Runners.
- Trigger a smoke run by hand: **Actions → soak → Run workflow**, set
  *secs* to e.g. `30`, and confirm the job is picked up by this runner, runs
  `tools/ci/soak.sh`, and uploads a `soak-logs-*` artifact.
- A real nightly run fails closed on any crash/hang (§19.6/§19.7); pull the
  `soak-logs-*` artifact for the failing reproducer.

## Maintenance

- Keep the runner application updated (GitHub deprecates old runner versions);
  `./svc.sh stop`, re-extract a newer tarball over it, `./svc.sh start`.
- To retire a runner: `sudo ./svc.sh uninstall` then
  `./config.sh remove --token <token>`.
