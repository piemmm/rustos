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

## What `cargo xtask ci` runs

| Step          | What it does                                                |
| ------------- | ----------------------------------------------------------- |
| `fmt`         | `cargo fmt --all -- --check`                                |
| `clippy`      | `cargo clippy --workspace --all-targets -- -D warnings`     |
| `deps-check`  | Enforces the [§17.4 modularity graph][modularity]           |
| `cfg-check`   | Rejects target-conditional `cfg` outside the arch ports     |
| `test`        | `cargo test --workspace --all-targets` + QEMU matrix, run once ([§7][test])                          |
| `docs-check`  | `cargo doc` (deny warnings) + `mdbook build` (link checked) |
| `deny`        | `cargo deny --all-features check` (license + advisory)      |
| `supply-chain`| Source-hash allow-list + RUSTSEC advisory SLA ([§19.3][sc]) |
| `fuzz --once` | Runs each fuzz harness once, fresh+logged seed ([§19.6][fz]) |
| `abi-check`   | Cross-checks the kernel syscall table against `lib/abi`     |
| `image`       | Builds every delivered image profile end-to-end (`debug` and `installer` for each image platform), so an image-breaking change cannot land green |

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
[test]: https://github.com/tairix-project/tairix/blob/main/AGENTS.md
