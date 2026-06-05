# Fuzzing the untrusted-input surface

Every decoder that ingests bytes from a possibly hostile peer is an
attack surface. `AGENTS.md` §19.6 requires that every IPC endpoint,
every syscall, every parser of untrusted input, and every public
`lib/abi` decoder carry a fuzz target, that each target run for at least
5 s on every pull request, and that the nightly soak run each for at
least 24 h. Any crash, hang, or sanitiser report blocks the next
release. The short per-PR budget is a deliberate practicality
concession; the nightly soak is where the real coverage comes from.

## In-tree harnesses, no external runner

RustOS does not pull in an external fuzz runner. §19.6 explicitly
sanctions an "equivalent in-tree harness", and `AGENTS.md` §2.12
("roll your own") makes that the default: every dependency widens the
trusted computing base. Each harness is therefore an ordinary
`cargo test` integration test driven by a small, seeded,
allocation-free PRNG. The seed is chosen by the orchestrator (see
[Seeding](#seeding-deterministic-ci-progressing-soaks) below): fixed for
a plain `cargo test` so the smoke sweep is reproducible, and fresh per
run under `cargo xtask fuzz` so consecutive soaks explore new inputs
instead of replaying the same stream. A flaky fuzz target is a bug
(`AGENTS.md` §7).

Five harnesses exist today:

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
* `kernel/mem/tests/fuzz_swap.rs` — the encrypted-swap restore path
  (`EncryptedSwap::load`), which reads records off a swap *device* whose
  bytes an attacker with disk access may have rewritten (`AGENTS.md` §4).
  It drives random pages, slots, and byte-level tampering and asserts
  that an untampered round-trip is faithful, that any tampering or
  relocation makes `load` fail closed, and that the output buffer is
  zeroed on every failure.

This completes the §19.6 burn-down's coverage of the IPC endpoints and
the `userland/net` protocol parsers. Future untrusted-input parsers
(font, image, archive, media — §19.5) each gain a harness as they land.

## Two run modes

A plain `cargo test` runs each harness as a fast, fixed-iteration smoke
sweep (100 000 inputs) so the normal suite stays quick and fully
deterministic. The dedicated orchestrator turns the same harnesses into
wall-clock runs:

```text
cargo xtask fuzz            # --quick: ≥ 5 s per harness (the CI budget)
cargo xtask fuzz --soak     # ≥ 24 h per harness (nightly)
cargo xtask fuzz --list     # list the registered harnesses
cargo xtask fuzz --target fuzz_decode   # run one harness
cargo xtask fuzz --secs 5   # custom budget (local iteration / tests)
cargo xtask fuzz --seed 42  # reproduce a logged run's input stream
```

The orchestrator exports `RUSTOS_FUZZ_BUDGET_SECS`. A harness reads it
and, when it is a positive value, keeps drawing fresh inputs from the
*same continuing* PRNG stream until the budget elapses; when it is unset
the harness runs the fixed smoke sweep.

## Seeding: deterministic CI, progressing soaks

A budget alone is not enough. If every run started the PRNG from the
same fixed seed, a 24 h soak would merely replay the first *N* inputs of
the identical stream the previous soak already explored — running longer
would find nothing new (`AGENTS.md` §2.1 forbids that kind of
busy-work). So the orchestrator also chooses a **per-harness seed** and
exports it as `RUSTOS_FUZZ_SEED`:

* By default it draws a *fresh* seed for each harness from host entropy
  (wall-clock time, pid, a monotonic counter — see
  `tools/xtask/src/commands/seed.rs`), so two soaks never run the same
  stream and coverage genuinely progresses night after night.
* `--seed N` instead derives a *deterministic* per-harness seed from
  `N`, so a crash a soak reported can be replayed exactly.

Every run logs the seed it picked (it is part of each job's label), so a
non-deterministic soak stays reproducible: feed the logged value back
via `--seed`. A plain `cargo test` leaves `RUSTOS_FUZZ_SEED` unset and
each harness falls back to its built-in constant, keeping the normal
suite fully deterministic. This is a *test-input* seed, not a security
seed; it deliberately does not go through `lib/crypto`/`lib/rng` (those
govern the kernel CSPRNG, §22).

The stateful proptest models (`AGENTS.md` §19.7) follow the identical
pattern through `cargo xtask proptest --seed N` and
`RUSTOS_PROPTEST_SEED`.

## CI integration

`cargo xtask fuzz --quick` is part of `cargo xtask ci`, running right
after the supply-chain gate. It **fails closed**: a crash, hang, or
invariant failure in any harness fails the pipeline, so a regression in
a decoder cannot merge. The 24-hour `--soak` mode is run by the nightly
job outside `ci`.

## Regression corpus

A crashing input found by a harness is added to that crate's regression
corpus alongside a unit test, so the same bytes are replayed
deterministically on every subsequent run (`AGENTS.md` §19.6).

`lib/abi/tests/regression_corpus.rs` is the seeded corpus for the
CCOMPAT CC3/CC4 decoders — the `rxe` needed-library table
(`NeededLibrary::decode`) and whole-image loader (`LoadImage::parse`)
from stage CC4, and the program startup vector (`ProcessStart::parse`
and the `ProcessStartHeader` / `StringSlot` field readers) from stage
CC3 (`plans/CCOMPAT.md`). No crash has been found in these decoders, so
the corpus is seeded with hand-crafted boundary cases that ring each
decoder's accept/reject edge: every entry is replayed through the same
"must not panic, and an accepted decode round-trips" contract the fuzz
harness enforces, and the *validating* decoders' accept/reject verdicts
are pinned by dedicated tests. A future crashing input is appended to
that file with a name and a verdict test rather than left only in the
PRNG stream. No other crash has been found to date.
