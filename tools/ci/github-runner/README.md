# `tools/ci/github-runner` — self-hosted GitHub Actions runner (Linux)

This directory documents standing up a **self-hosted GitHub Actions runner on
Linux** for RustOS. It is the GitHub-native counterpart of the
cron/systemd/launchd builders in the parent `tools/ci/` directory: same
`cargo xtask` pipeline, just triggered by GitHub instead of a local timer.

## Why a self-hosted runner is needed

There are two CI workloads, and they want different runners:

| Workload | Workflow | Runner | Why |
|----------|----------|--------|-----|
| Per-PR / per-push gate (`cargo xtask ci`) | `.github/workflows/ci.yml` | GitHub-hosted `ubuntu-latest` | Free, ephemeral, finishes well inside the hosted time cap. |
| Nightly 24 h soaks (§19.6 fuzz, §19.7 proptest) | `.github/workflows/soak.yml` | **self-hosted `[self-hosted, linux]`** | A 24 h job exceeds the GitHub-hosted per-job time cap; it must run on a machine we own. |

`soak.yml` runs `tools/ci/soak.sh`, which fans every fuzz harness and proptest
model out into parallel `--soak` processes sharing one 24 h wall clock. That is
the same script a standalone cron/systemd/launchd builder runs, so this runner
and a standalone builder produce equivalent results.

## Labels

`soak.yml` targets `runs-on: [self-hosted, linux]`. `self-hosted` and `linux`
are applied automatically by `config.sh` on a Linux host, so no custom label is
required. If you dedicate a box to soaks, add an extra label (e.g. `soak`) and
narrow `runs-on` to match.

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
  --labels self-hosted,linux \
  --unattended --replace
```

## Run it as a service (survives logout and reboot)

The runner ships its own systemd integration; use it rather than hand-writing a
unit:

```sh
sudo ./svc.sh install "$USER"   # generate + enable a systemd service for this user
sudo ./svc.sh start             # start it now
sudo ./svc.sh status            # verify it is listening for jobs
```

`svc.sh install` creates a systemd service that auto-restarts and starts on
boot, so the host is ready for the 02:00 UTC nightly `soak.yml` trigger without
anyone logged in. (This is separate from the `tools/ci/systemd/` units, which
drive a *standalone* builder; you want one or the other, not both, on a given
host.)

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
