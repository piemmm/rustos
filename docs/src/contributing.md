# Contributing

All contributors — human or AI — must read the [`AGENTS.md`][agents] charter
before opening a pull request. It is binding; this page does not restate its
rules, it points to them.

## Workflow

1. Pick a stage from [`PLAN.md`][plan]. Do not begin a stage before its
   listed dependencies are complete.
2. Discuss non-trivial design changes in an issue first. Inventing public
   interfaces is forbidden; extend versioned ones instead.
3. Run `cargo xtask ci` locally before pushing. The same command runs in
   CI and must be green. It runs each test exactly once, on a developer
   machine and a CI runner alike ([§7][test]); the flake-hunting
   repetition lives in the time-limited GitHub soaks, not in `ci`.
4. Update documentation in the same commit as the code it describes —
   rustdoc on every public item and the relevant page in `docs/src/`.

## Flaky tests are defects — fix them, never re-run them

A test that fails intermittently is a **bug**, and it is fixed like any other
bug ([§7][test], [§2.5][agents], [§2.18][agents]). This is binding and has no
exceptions:

- **"Machine load" is never an excuse.** Do **not** dismiss a failure as
  "flaky because the machine was busy", "CPU contention", "an oversubscribed
  host", "a slow CI runner", or "it passes when I run it on its own".
  Re-running a failed test in isolation until it goes green is **not** an
  investigation and **not** a fix — it is the exact get-out the charter forbids.
- **Load exposes real bugs; it does not cause false failures.** Every time a
  failure in this project has been blamed on machine load, it has turned out to
  be a genuine defect — a race, an unsynchronised wait, an unbounded queue, a
  budget sized to an idle host, a missing completion signal — that the load
  merely revealed. Treat a failure that appears under load as a confirmed
  defect and find its root cause.
- **A green re-run is not evidence the defect is gone.** It proves only that
  the failure is intermittent, which is precisely the bug. Diagnose the *why*,
  fix the code or the test so it cannot recur under any load, and add a
  regression test ([§7][test]).
- **A load-dependent timeout is fixed structurally**, not retried: size the
  budget to the actual work, bound concurrency so guests do not oversubscribe
  the host, or add a completion signal ([§7][test]). See the CI soak notes in
  `tools/ci/README.md`.

Do not report work as done while any test has failed even once during the
change. If the real fix is genuinely too large for the current change, stop and
ask — never wave the failure through as transient, load, or environment.

## What `cargo xtask ci` runs

| Step          | What it does                                                |
| ------------- | ----------------------------------------------------------- |
| `fmt`         | `cargo fmt --all -- --check`                                |
| `clippy`      | `-D warnings` for the host **and once per Tier-1 target** (see below) |
| `deps-check`  | Enforces the [§17.4 modularity graph][modularity]           |
| `cfg-check`   | Rejects target-conditional `cfg` outside the arch ports     |
| `charter-cite`| Rejects a comment or package description citing a charter section instead of the reason ([§2.11][cite]) |
| `test`        | `cargo test --workspace --all-targets` + QEMU matrix, run once ([§7][test])                          |
| `docs-check`  | `cargo doc` (deny warnings) + `mdbook build` (link checked) |
| `deny`        | `cargo deny --all-features check` (license + advisory)      |
| `supply-chain`| Source-hash allow-list + RUSTSEC advisory SLA ([§19.3][sc]) |
| `fuzz --once` | Runs each fuzz harness once, fresh+logged seed ([§19.6][fz]) |
| `abi-check`   | Cross-checks the kernel syscall table against `lib/abi`     |
| `image`       | Builds every delivered image profile end-to-end (`debug` and `installer` for each image platform), so an image-breaking change cannot land green |

## `clippy` lints every target, not just the host

A host-only `cargo clippy --workspace --all-targets` lints almost none of the
code that actually ships. A kernel subsystem, an architecture backend, a
driver, a system service and an application body are compiled only when their
crate is built for a bare-metal triple — most of them behind the `freestanding`
cfg each crate's `build.rs` sets when the target OS is `none`, whose host arm is
an inert stub. The image and QEMU stages then compile those bodies but never
lint them, so a lint in shipped code could not fail CI.

`clippy` therefore runs the same `-D warnings` pass once per target:

| Pass | What it covers |
| ---- | -------------- |
| host | `--workspace --all-targets`, including every unit-test target |
| each of the three freestanding Tier-1 triples, **once per stratum** | `kernel/*`, then `lib/*`, then `drivers/*` + `userland/*` — every workspace member the image pipeline cross-compiles, less host-only `tools/*`, less `tests/*`, and less a foreign `kernel/arch/<other>` |
| `wasm32-unknown-unknown` | `kernel/arch/wasm32` + `kernel/arch/api` (the only product code the browser target builds), and the browser verticals |

Every selection is *derived* — from the workspace member list and the wasm
vertical table — so a new crate or vertical is linted without being added to a
second list. `--all-targets` is absent from the target passes because a
bare-metal target has no test harness to link one against; the host pass covers
those.

The enrolled **QEMU guests** under `tests/integration/` are test support rather
than product and are deliberately *not* in this gate; that gap is staged in
`plans/CODEVERIFY.md`.

The stratum split is load-bearing, not cosmetic. Cargo unifies features across
every package named in one invocation, so naming the kernel binary alongside
the userland programs turns on the `program` features of their shared
dependencies and links `lib/rt`'s `#[global_allocator]` and `#[panic_handler]`
into the kernel — a duplicate `panic_impl` lang item. The image pipeline builds
the kernel and the programs separately for the same reason.

## Every step is time-limited

No pipeline step can run forever. Every external command `xtask` spawns is
given a wall-clock budget; when a step overruns it, its whole process group is
signalled — `SIGTERM`, a short grace period, then `SIGKILL` — and the step
fails, naming itself and its budget.

Killing the *group* rather than the direct child is the point: a `cargo` step
is really the rustc, test-binary and QEMU processes it spawns, and killing only
the child would leave those running, holding the build lock and the terminal.

An overrun is a hard failure. It is never retried and never folds into a
passing result, because a step that hangs is a defect in exactly the way the
previous section describes — an unbounded wait, a missing completion signal, a
budget sized to an idle host — not a nuisance to paper over.

Ordinary steps share one default budget; a step known in advance to need
longer (the image gate, the QEMU matrix build) asks for a larger one
explicitly. A slow machine can raise every budget at once with
`TAIRIX_XTASK_TIMEOUT_SECS=<seconds>`; the override only ever *raises* a
budget, so it cannot silently shorten one, and a malformed value is rejected
rather than quietly ignored. The QEMU verticals keep their own per-guest
budgets — this is an outer backstop, not a replacement for them.

Other subcommands (`build`, `clean`, `prune`, `coverage`) exist for
development and release flows; they are documented by `cargo xtask --help`.

`cargo xtask clean` reclaims the `target/` directory, which grows into
tens of gigabytes per target because `-Z build-std` rebuilds the whole
standard library for each of the four bare-metal Tier-1 targets. It
delegates to `cargo clean` (honouring `$CARGO_TARGET_DIR`), forwards the
usual cargo selectors (`--release`, `--doc`, `--target <triple>`,
`-p <crate>`) to scope the clean, and reports how much space was freed.

`cargo xtask prune` reclaims only the *superseded* build-script output
that dominates that growth. The `tairix-kernel` build script compiles the
embedded userland programs — each a roughly 1 GB `-Z build-std` tree —
into an `OUT_DIR` cargo keys by build-script fingerprint, so every
`build.rs` change strands the previous tree under
`target/<triple>/<profile>/build/` forever. `prune` keeps the newest
`build/<pkg>-<hash>` directory per package (the live one) and removes the
older siblings and their `.fingerprint` entries. Unlike `clean` it never
touches the current build, so the next compile stays incremental — which
is why it runs automatically before every `build` and `image`.

[agents]: https://github.com/tairix-project/tairix/blob/main/AGENTS.md
[plan]: https://github.com/tairix-project/tairix/blob/main/PLAN.md
[modularity]: ./architecture/modularity.md
[sc]: ./security/supply_chain.md
[fz]: ./security/fuzzing.md
[cite]: https://github.com/tairix-project/tairix/blob/main/AGENTS.md
[test]: https://github.com/tairix-project/tairix/blob/main/AGENTS.md
