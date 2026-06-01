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
   CI and must be green. Locally it runs the test matrix once; on the
   GitHub Actions runners it repeats the matrix 20× to hunt flaky tests
   ([§7][test]), so a local `ci` finishes quickly without that hours-long
   repeat.
4. Update documentation in the same commit as the code it describes —
   rustdoc on every public item and the relevant page in `docs/src/`.

## What `cargo xtask ci` runs

| Step          | What it does                                                |
| ------------- | ----------------------------------------------------------- |
| `fmt`         | `cargo fmt --all -- --check`                                |
| `clippy`      | `cargo clippy --workspace --all-targets -- -D warnings`     |
| `deps-check`  | Enforces the [§17.4 modularity graph][modularity]           |
| `cfg-check`   | Rejects target-conditional `cfg` outside the arch ports     |
| `test`        | `cargo test --workspace --all-targets` + QEMU matrix; repeated 20× on GitHub Actions to catch flaky tests, once locally ([§7][test]) |
| `docs-check`  | `cargo doc` (deny warnings) + `mdbook build` (link checked) |
| `deny`        | `cargo deny --all-features check` (license + advisory)      |
| `supply-chain`| Source-hash allow-list + RUSTSEC advisory SLA ([§19.3][sc]) |
| `fuzz --quick`| Runs each in-tree fuzz harness ≥ 5 s ([§19.6][fz])          |
| `abi-check`   | Cross-checks the kernel syscall table against `lib/abi`     |

Other subcommands (`build`, `coverage`, `image`) exist for development and
release flows; they are documented by `cargo xtask --help`.

[agents]: https://github.com/rustos-project/rustos/blob/main/AGENTS.md
[plan]: https://github.com/rustos-project/rustos/blob/main/PLAN.md
[modularity]: ./architecture/modularity.md
[sc]: ./security/supply_chain.md
[fz]: ./security/fuzzing.md
[test]: https://github.com/rustos-project/rustos/blob/main/AGENTS.md
