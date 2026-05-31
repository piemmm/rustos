# Fuzzing the untrusted-input surface

Every decoder that ingests bytes from a possibly hostile peer is an
attack surface. `AGENTS.md` §19.6 requires that every IPC endpoint,
every syscall, every parser of untrusted input, and every public
`lib/abi` decoder carry a fuzz target, that each target run for at least
60 s on every pull request, and that the nightly soak run each for at
least 24 h. Any crash, hang, or sanitiser report blocks the next
release.

## In-tree harnesses, no external runner

RustOS does not pull in an external fuzz runner. §19.6 explicitly
sanctions an "equivalent in-tree harness", and `AGENTS.md` §2.12
("roll your own") makes that the default: every dependency widens the
trusted computing base. Each harness is therefore an ordinary,
deterministic `cargo test` integration test driven by a small, seeded,
allocation-free PRNG. A fixed seed makes any failure reproducible — a
flaky fuzz target is a bug (`AGENTS.md` §7).

Four harnesses exist today:

* `lib/abi/tests/fuzz_decode.rs` — the `lib/abi` wire decoders
  (`IpcMessageHeader::from_bytes`, `ManifestHeader::from_bytes`). It
  asserts no input panics and that any accepted header round-trips
  through its encoder unchanged.
* `kernel/syscall/tests/fuzz_args.rs` — the syscall dispatcher's
  per-argument validation. It cross-checks the dispatcher against an
  independent acceptance mirror over random `(syscall, RawArgs)` pairs.
* `userland/net/icmp/tests/fuzz_parse.rs` — the `userland/net` protocol
  parsers (Ethernet, ARP, IPv4, ICMP echo) plus the composed
  `Responder::handle_frame` and `Client` classifiers. It asserts no
  input panics, that any accepted decode round-trips through its
  encoder, and that any reply the responder emits fits the caller's
  buffer and is itself a well-formed frame.
* `kernel/ipc/tests/fuzz_port.rs` — the capability-checked IPC port
  endpoint. It drives random `(sender capabilities, payload)` pairs at
  `Port::send` and asserts the fail-closed decision against an
  independent mirror (the dispatcher's capability → size → capacity
  precedence), that a delivered message round-trips through `recv`
  byte-for-byte in FIFO order, and that the mailbox never exceeds its
  capacity. A separate test proves a closed port refuses every sender,
  however privileged.

This completes the §19.6 burn-down's coverage of the IPC endpoints and
the `userland/net` protocol parsers. Future untrusted-input parsers
(font, image, archive, media — §19.5) each gain a harness as they land.

## Two run modes

A plain `cargo test` runs each harness as a fast, fixed-iteration smoke
sweep (100 000 inputs) so the normal suite stays quick and fully
deterministic. The dedicated orchestrator turns the same harnesses into
wall-clock runs:

```text
cargo xtask fuzz            # --quick: ≥ 60 s per harness (the CI budget)
cargo xtask fuzz --soak     # ≥ 24 h per harness (nightly)
cargo xtask fuzz --list     # list the registered harnesses
cargo xtask fuzz --target fuzz_decode   # run one harness
cargo xtask fuzz --secs 5   # custom budget (local iteration / tests)
```

The orchestrator exports `RUSTOS_FUZZ_BUDGET_SECS`. A harness reads it
and, when it is a positive value, keeps drawing fresh inputs from the
*same continuing* PRNG stream until the budget elapses; when it is unset
the harness runs the fixed smoke sweep. Because the seed is fixed, a
crash at draw *N* is reproducible regardless of how far a given machine
got within the budget.

## CI integration

`cargo xtask fuzz --quick` is part of `cargo xtask ci`, running right
after the supply-chain gate. It **fails closed**: a crash, hang, or
invariant failure in any harness fails the pipeline, so a regression in
a decoder cannot merge. The 24-hour `--soak` mode is run by the nightly
job outside `ci`.

## Regression corpus

A crashing input found by a harness is added to that crate's regression
corpus alongside a unit test, so the same bytes are replayed
deterministically on every subsequent run (`AGENTS.md` §19.6). No crash
has been found to date, so no corpus entry exists yet.
