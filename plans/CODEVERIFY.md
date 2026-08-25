# CODEVERIFY.md — Find and fix bad code across the workspace

This is a staged plan to systematically hunt for code-quality and charter
violations across every `.rs` file in the workspace and fix each one to a
senior-engineer standard. It is **binding under `AGENTS.md`**; read `AGENTS.md`
and `PLAN.md` first. Every rule in both applies here without exception.

The work runs as a repeating loop across many fresh AI contexts. A single
context does a bounded amount of work: it finds (or picks up) **one** issue,
understands it fully, fixes it properly with tests, runs the whole-project gate
green, and records the remaining backlog in this plan so
the next context can continue. The loop ends when no violations remain.

## 0. Scope and decisions (binding for this plan)

- **One issue per context, done completely.** A context fixes a single issue
  (or one tight cluster of the *same* defect) end to end — find, understand,
  fix, test, green gate, document — rather than half-fixing many. This keeps
  each diff reviewable (§14) and fits a single AI context window.
- **Quality bar is §2.6 / §23.** A fix is acceptable only if the agent
  understood the code it changed, its callers, and its callees, and would
  defend every line in review without embarrassment. A fix that compiles and
  passes tests but is not understood is **not** acceptable — escalate (§15.7)
  instead of guessing.
- **Correctness over volume.** Never weaken a test, silence a lint, or paper
  over a defect to "make it pass" (§2.1, §2.5, §15.3, §15.9). A misread issue
  is escalated, not bluffed.
- **No behaviour change without its test.** Every fix that changes behaviour
  lands with a regression test that fails before and passes after (§7, §2.18,
  §23.4); a fuzzer/proptest find also enters the corpus (§19.6).
- **The backlog lives in this plan**, beside the methodology: it is both the
  re-runnable prompt for the next context and the running issue queue, and it
  is the only handoff artefact. A fixed issue's entry is deleted rather than
  annotated, so the plan states the remaining work and never accrues per-issue
  history (§13).
- **This is not a comment refactor.** Comment "waffle" reduction is owned by
  `plans/WAFFLE.md`; do not conflate the two. CODEVERIFY targets *defects and
  bad code*, not comment style.
- **The gate lints bare-metal *product* code, so the lint-catchable classes
  need no manual hunt there.** `cargo xtask clippy` runs `-D warnings` for the
  host *and* once per Tier-1 target, so a `freestanding` body in `kernel/*`,
  `lib/*`, `drivers/*` or `userland/*` is linted rather than only compiled.
  Scan those for what a lint cannot see — wrong invariants, ambient authority,
  duplication, dead code — not for what the gate already rejects.
- **`tests/*` and `tools/*` bodies are the exception: nothing lints them for a
  bare-metal target.** The per-target passes exclude them by design
  (`docs/src/contributing.md`), and the QEMU matrix only *builds* the fixtures,
  without `-D warnings`. A dead import or an `unused_mut` in a fixture body
  therefore survives a green gate, and only `cargo check --target <its target>`
  over the enrolment table in `tools/xtask/src/commands/qemu_tests.rs` sees it.
  Run that when a change touches fixture source; `cargo build --workspace`
  compiles only the host stubs.

## 1. What counts as a violation (the hunt checklist)

Applied while reading code. Any one of these is an issue to record and fix.
This list is the charter restated as a review checklist — it is not exhaustive;
"generally bad code a senior engineer would reject" (§2.6) is in scope even if
it is not enumerated here.

### Safety / panics (§2.9, §2.1)

- `panic!`, `unreachable!`, `todo!`, `unimplemented!`, `assert!`/`debug_assert!`
  used as control flow, or arithmetic that can panic (indexing, `+`/`-`/`*`
  overflow, slicing, fallible `from`/`into`) on a **production** path — anything
  not a test and not a documented `// SAFETY-INVARIANT:` boot-time invariant.
- `.unwrap()` / `.expect()` outside `#[cfg(test)]` / `tests/`.
- Allocation or `Result` failure handled by panic rather than as a value (§4).

### Hacks and shortcuts (§2.1, §2.17, §15.10)

- `// TODO`, `// FIXME`, `// HACK`, `// works for now`, sleep/retry-until-it-works
  loops, busy-spins, commented-out code (also §2.14).
- `#[allow(...)]` without a justification comment tying it to a real invariant.
- A limit bumped (buffer/stack/quota enlarged) in place of a structural fix
  (§2.17).

### Security (§5.4, §2.7, §23.1)

- An entry point (syscall / IPC method / driver entry / parser / FS op) that
  checks capability *after* touching state, trusts a caller-supplied identity,
  skips validating any input field, fails *open*, or grants authority it was
  not delegated (ambient authority, §4).
- `unsafe` without an accurate `// SAFETY:` block, or whose invariant does not
  actually hold for all inputs, or that has no test/model exercising it (§2.10).
- A secret/key/capability token not zeroed on free, or reaching swap/logs/fd 3
  (§4, §19.4, §20).
- A parser of untrusted input not behind the §19.5 sandbox / without a fuzz
  harness (§19.6).

### Duplication, dead code, bloat, creep (§2.2, §2.3, §2.4, §2.14)

- Copy-pasted logic that should be a `lib/*` crate or shared function (but
  **not** the sibling-implementation carve-out, §2.2).
- Dead code: unused items, `_old`/`legacy_`/`unused_` names,
  `#[allow(dead_code)]`, orphaned files/fixtures/doc pages.
- Speculative public surface added "for later" with no present caller (§2.4),
  one-line "convenience" wrappers (§2.3, §15.5).

### Correctness, concurrency, layering, scaling (§23.2, §17.4, §24)

- Data races / torn reads / "works on one core" assumptions; lock-ordering that
  can deadlock; shared state not behind `lib/sync` (§4).
- Error paths that leak (memory, locks, capabilities, handles), or skip cleanup.
- `cfg(target_arch …)` / `cfg(target_pointer_width …)` outside the §17.2
  allow-list; a non-GUI crate depending on `userland/gui/*` (§17.3); any §17.4
  layering reversal.
- Absolute time stored as 32-bit / `usize` / `time_t` in an ABI or persisted
  format (§21).
- A hand-picked `const` capacity ceiling that a larger machine outgrows or a
  smaller one wastes, where it should be derived/grown (§24.1) — *not* a
  fixed security/format bound (§24.4).

### Performance (§2.16, §23.2)

- Gratuitous waste on a hot path (syscall/IPC dispatch, capability check,
  scheduler, context switch, allocator, compositor, FS/network data paths):
  needless allocation/copy in a loop, a lock held too long, O(n²) where O(n)
  is as clear. Back any regression claim with measurement (§2.16); never reach
  for `unsafe` for speculative speed.

### Tests and docs (§7, §2.8, §13)

- A public item without rustdoc; a `docs/src/` page out of sync with code;
  a stale symbol reference.
- A weakened/`#[ignore]`d/deleted test, a flaky test, or a fixed bug with no
  regression test. A flaky test is a defect regardless of how it is perceived:
  a failure excused as "machine load", CPU contention, an oversubscribed host,
  a slow runner, or "it passes when run on its own", or "resolved" by re-running
  it in isolation, is the exact get-out the charter forbids (§7) — it is
  diagnosed to root cause and fixed structurally, with a regression test.

## 2. Method for one issue (binding per context)

1. **Pick the issue.** If this plan lists an unfixed,
   ready issue, take the top one. Otherwise scan an unfinished area (§3) with
   `search_project` for the patterns in §1 and stop at the **first** genuine
   violation, then record it.
2. **Understand it fully before touching anything.** Read the offending code,
   *every* caller of it, and *everything it calls*, until the intended
   behaviour and invariants are clear. If understanding is not reachable from
   the repository, **stop and ask** (§15.7) — do not guess (§2.1, §15.7).
   Confirm it is a real defect, not an intended carve-out (e.g. the §2.2
   sibling implementations, a justified `#[allow]`, a test-only `unwrap`).
3. **Design the proper fix.** Prefer making illegal states unrepresentable
   (§2.11) over adding a runtime guard; prefer a structural control over a
   limit bump (§2.17); never introduce a compat shim or dead code (§2.13,
   §2.14). If the clean fix is genuinely larger than one context can do well,
   record it as escalated (§15.7) and pick a smaller issue instead — do **not**
   land a partial or low-quality fix.
4. **Implement** the minimal correct change, updating every caller in the same
   change (§2.13). Update rustdoc and the relevant `docs/src/` page in the same
   change (§2.8, §13).
5. **Add the regression test** that fails before and passes after (§7, §2.18).
   For a fuzzer/proptest find, add the input to the corpus (§19.6).
6. **Run the whole-project gate green** (§4 below) and quote the output.
7. **Self-review under §23** adversarially; state the verdict.
8. **Update this plan**: mark the fixed issue done (delete
   its entry — git is the history, §13), and append any *new* violations
   noticed in passing as backlog entries (§2.18: notice it → record it, never
   stay silent). Leave the file ready for the next context.

## 3. Work breakdown (scan areas / stages)

The hunt is staged by workspace area so coverage is trackable and a context
knows where to look next. Areas are scanned roughly highest-risk first; within
an area, fix issues one per context until the area is clean, then move on. An
area is `done` only when a full §1 scan of it surfaces no remaining violation.

Status keys: `planned` / `in progress` / `done` / `blocked`.

### Stage V1 — `kernel/*` and `lib/caps`, `lib/crypto`, `lib/sync` — Status: planned

The capability-critical and `unsafe`-dense core (allocator, paging, arch ports,
context switch, scheduler, syscall/IPC dispatch, `lib/caps`, `lib/crypto`,
`lib/sync`). Highest blast radius; §23.1 security review and §2.10 `unsafe`
review apply most strictly here. Coverage floor ≥ 95% for the §7-listed crates.

### Stage V2 — remaining `lib/*` — Status: planned

All other shared crates. Watch for duplication that belongs behind one crate
(§2.2), missing rustdoc (§6), and untrusted-input parsers without sandboxing /
fuzzing (`lib/vt`, `lib/fdt`, `lib/svg`, `lib/font`, `lib/compress`).

### Stage V3 — `drivers/*` — Status: planned

All driver crates. Watch for ambient authority (§4/§18.5), entry points that
validate input incompletely (§5.4), and `cfg(target_arch …)` that should live
behind the Arch HAL (§17.2).

### Stage V4 — `userland/*` — Status: planned

All userland crates (`system`, `session`, `shell`, `gui`, `apps`, `net`). The
known seed issue (the shell parser `unreachable!`) lives here; see the
backlog above.

### Stage V5 — `tools/*` and `tests/*` — Status: planned

`tools/xtask`, `tools/qemu`, `tools/cc`, `tools/mkimage`, `tools/ci`;
`tests/fuzzseed` and `tests/integration/*`. Host-only build glue: still held to
§2.6, but the §2.9 production-path panic rule is read against these being
build/test tooling, not shipped kernel/userland paths.

## 4. Definition of done (per issue and overall)

An issue fix is done only when, run from the repository root over the **entire**
workspace (§7, §2.15, §15.6, never a `-p` subset):

- `cargo fmt --all` (and `cargo fmt --all --check`),
- the full `cargo xtask ci` pipeline (clippy `-D warnings`, the test matrix,
  `docs-check`, `deny`, cfg/deps/abi checks, the per-PR fuzz/proptest gates,
  model-check, spec-review, crypto constant-time),
- `cargo xtask fuzz --secs 5`,
- anything else `.github/workflows/ci.yml` exercises (e.g. `tools/ci/soak.sh`,
  which on a developer machine (that's us) runs for a **maximum of 20 seconds**
  — `tools/ci/soak.sh both --secs 20` — never the unbounded 24 h soak, which is
  the CI/soak host's job),

all pass; the §23 self-review confirms the fix is correct, understood, tested,
and leaves no dead code or compat shim; and this plan has
been updated. The actual gate output is quoted in the completion report.

The overall task is done when V1–V5 are all `done` — a clean §1 scan of the
whole workspace surfaces no violation — and this plan
records an empty backlog.


---

## Open sweep: the charter-compliance audit of the recent commit range

The twenty-five commits ending at `a66f6a27` landed roughly 60 000 lines across
545 files without the charter applied throughout, so that surface and the code
around it are being re-reviewed adversarially under §23. The scan matrix below
is the audit's coverage record; the areas marked open are the remaining work.

Clean on the commit-range surface, verified by scan and needing no further
pass: production-path `panic!`/`unwrap`/`expect` (§2.9 — every hit is a test,
host build script, or fixture), `TODO`/`FIXME`/`HACK`/"for now" markers (§2.1,
§2.19), `cfg(target_arch …)`/`cfg(target_pointer_width …)` outside the §17.2
allow-list, and 32-bit absolute time in an ABI or persisted format (§21).
Also clean, and worth re-checking only when fixture source changes: rustc
warnings in the freestanding fixture bodies — a `cargo check --target` over all
149 enrolled package/target pairs reports none.

### Done — every `#[allow(...)]` states an invariant, and three defects it hid

The sweep found 135 sites with no justification (the earlier 168 counted
comments placed *below* the attribute, and `reason = "…"` inside it, as absent;
both are real justifications). Each was decided per site: an invariant stated,
a checked conversion put in its place, or the types fixed so the cast
disappeared. Nothing was blanket-annotated, and a tree-wide scan now reports
zero.

Three of them were hiding defects, all with regression cover:

- **The syscall-number register was truncated, so a reserved-bit probe ran a
  real syscall.** `dispatch_via_slot` narrowed the caller's 64-bit number
  register with `as u16`, so `x8 = 0x1_0000` executed `yield` (0) and
  `0x1_0001` executed `exit` — while the `#[allow]`'s comment claimed the
  opposite ("a value above `u16::MAX` is rejected by the dispatcher … fail
  closed"). No `SyscallUnknown` record was emitted either, so probing above the
  identifier space was invisible in the audit trail. The narrowing is gone:
  `SyscallNumber::from_register` is the one checked decode from a raw register,
  `DispatchHook::dispatch` and `Dispatcher::dispatch` take the register whole,
  and the single validation point is the one that already owned the refusal and
  its audit event. `audit_unknown` now logs all 64 bits, so an aliasing probe is
  distinguishable from the low value it wore.
- **Two `#[allow]`s were silencing nothing at all** — `queue_max_size` in
  `lib/virtio/src/transport_mmio.rs` and the migration block in
  `kernel/sched/eevdf`, neither of which contains a cast. Removed; the crates
  lint clean without them. This is the same false-justification class the
  `dead_code` sweep found fourteen of, so it is worth probing rather than
  trusting any remaining annotation.
- **A QEMU fixture could report PASS for a failing exit code.** The `EXIT`
  arm of four test kernels read the code as `args[0] as u16`, so a child
  exiting `0x1_0000` decoded to `0` — which `heap_qemu_aarch64` reads as
  success. The decode is now the dispatcher's own (`(args[0] & 0xFFFF_FFFF) as
  i32`) and saturates into the report. Twelve further sites composed
  `FAIL_* + (code as u16)`, which overflows `u16` for a large code: panicking
  in debug, or aliasing onto another fixture's code in release. All are
  `saturating_add` now.

Two structural wins came out of it rather than annotations: `Rect::center()`
in `lib/geometry` (five byte-identical copies of the same centre computation,
one of them production), and `SyscallNumber::from_register`, which replaced the
`number as u16` narrowing open-coded in nineteen QEMU test kernels — those
fixtures now validate the register exactly as production does, which is the
point of a fixture that mirrors the dispatcher.

Worth knowing for the next context: `cargo build --workspace` does not compile
the bare-metal fixture bodies, so a change to them is only checked by
`cargo check -p <crate> --target <its target>` (or the gate's per-target
clippy). A bare `cargo clippy --target … -- -D warnings` on those crates
reports ~10 pre-existing lints that the gate's own invocation does not; compare
counts against a reverted file before treating any as yours.

### Done — fixture finisher codes compose through one tested seam

The sixteen copies are gone. `tests/integration/finisher`
(`tairix-itest-finisher`) is the runtime fixture-support seam the bare-metal
binaries link — `no_std`, dependency-free, one function — and every fixture
reports through `fail_code(base, observed)`. It is a second crate rather than
a `harness` module because `harness` is a *build* dependency and links `std`;
nothing it holds can reach a running test kernel. It is not in `lib/*` because
fixture arithmetic is not production surface.

`fail_code` is generic over `TryInto<u16>` so the fifteen `i32` exit-code sites
and mem-pin's `u32` CPU-mask site share the one definition rather than adding a
per-type conversion at the call site.

Two properties beyond the deduplication, each with a test that fails without
its guard:

- **The composed code is never zero.** Both finishers read a zero code as
  *success*, so a composition reaching zero would report a failing run as a
  pass — the same class as the truncated `EXIT` decode this sweep already
  fixed. `fail_code` floors at 1, which closes it for every *computed* code.
- **Saturation is the top of the band, not a wrap.** The pre-seam form aborted
  in debug and aliased onto a smaller assigned code in release.

The four fixtures that implement the `EXIT` arm themselves no longer open-code
the register decode either; see the `lib/abi` entry below.

### Done — a finisher code is a `NonZeroU16`, so no failure can report a pass

`exit_failure` took a bare `u16`, and zero is each board's *success* status:
aarch64 passes the code as the semihosting `SYS_EXIT` subcode, riscv64 packs it
into the `sifive_test` high half, and both exit QEMU with status 0 — which the
runner reads as `Outcome::Pass`. The finisher whose whole job is reporting
failure failed **open** on one input.

The chosen shape is the unrepresentable one: `exit_failure(NonZeroU16)` on both
boards, `fail_word(NonZeroU16)`, and `fail_code` returning `NonZeroU16` from a
`NonZeroU16` base. The substitution alternative — mapping zero onto a reserved
code inside each board — was rejected: it catches the typo at run time instead
of build time, and it reports a code the fixture did not assign, which is the
aliasing this sweep already fixed once in `fail_code`'s saturation.

Fixtures mint their codes through `tairix_itest_finisher::fail_point!(n)`, a new
sibling of `fail_code` in the same crate. It is a macro rather than a `const fn`
so its body is an inline `const` block: a zero is a build error at *every* call,
not merely at the `const` initialisers that happen to force evaluation, so the
constructor has no runtime panic path. 541 constants across 54 fixture files
became `NonZeroU16`, 22 bare literals became named codes beside them, and 50
fixture manifests gained the `tairix-itest-finisher` edge (14 already had it).
No reported code changed value. x86_64's `exit_failure()` takes no code and is
untouched.

Four guards, each probed by removing it:

- **`fail_point!(0)`** is `error[E0080]: evaluation panicked: a zero finisher
  code reports a failing run as a pass`.
- **`exit_failure(0)`** is `error[E0308] … expected NonZero<u16>, found
  integer`, on both boards.
- **riscv64's encoding** is covered by
  `no_reportable_code_exits_with_the_pass_status`, which walks the whole
  `1..=0xFFFF` domain and asserts the status QEMU will report is never zero.
  Widening `fail_word` back to `u16` (which widens the loop to `0..=`) fails it
  on the first iteration.
- **`fail_code`'s floor is now its return type**, not a `.max(1)` a later edit
  could drop; `NonZeroU16::saturating_add` carries it. Reverting the pre-change
  `u16` shape and deleting the `.max(1)` fails
  `the_composed_code_is_never_the_success_status`.

Two things a later reviewer should not re-litigate:

- **aarch64 gets no encoding test, deliberately.** Its `SYS_EXIT` subcode *is*
  the host exit status, so the only thing a host test could assert is that a
  `NonZeroU16` is non-zero — a test of `core`, not of this code. The type is the
  whole guard there, which is what the module doc now says; riscv64 has a real
  encoding (`fail_word`) and therefore a real test. Adding an `exit_subcode`
  wrapper for symmetry would be a helper around `u64::from(code.get())`.
- **The `_program` fixtures' `FAIL_*` constants are `i32` process exit codes,
  not finisher codes**, and stay untouched. Both families live under
  `tests/integration` with the same naming, so a tree-wide rewrite of
  `const FAIL_…: u16` is only safe because the two never share a crate — worth
  re-checking before any similar sweep.

### Done — the `I32` argument slot has one decode, in `lib/abi`

`SyscallNumber::from_register` gave the syscall *number* register one checked
decode; the `I32` *argument* register still had six copies of the recovery in
`kernel/syscall/src/table.rs` (`exit`, `wait`, `signal`, `sched_set`,
`console_foreground`, and `validate_arg`), each with its own
`#[allow(clippy::cast_*)]` and a comment admitting it was "the same recovery
`EXIT` uses" — and four more in the QEMU test kernels that implement the `EXIT`
arm themselves, whose comment claimed the decode was "exactly as the dispatcher
decodes it" while nothing held it to that.

The rule now lives once, beside `AbiType`, as the two halves it actually has:
`i32_register_is_canonical` (the acceptance `validate_arg` applies) and
`i32_from_register` (the value an accepted register carries). Seven lint
suppressions became none — the canonical check is expressed over the sign
extension directly, so it needs no cast, and the recovery reinterprets through
`u32::cast_signed`.

The arms take the *infallible* recovery, not a `Result`: `validate_arg` has
already accepted the register by the time an arm reads it, so a fallible decode
there would be a dead error path — the shape the `hwtree.rs` item below calls
out.

Two things a later reviewer should not re-litigate:

- **`fuzz_args.rs`'s `arg_is_well_typed` still re-implements the rule, on
  purpose.** It is the harness's independent model of the dispatcher's
  acceptance; pointing it at the same function would make the cross-check
  vacuous. That independence is what proves the new bit arithmetic: the harness
  compares its round-trip formulation against the live dispatcher over random
  registers and fails on any divergence, so the two spellings of the rule are
  machine-checked equivalent rather than argued equivalent. Its `narrow_for`
  sibling builds a canonical register, which is the inverse operation and has
  one caller.
- **`i32_argument_must_be_sign_extended` only ever proved the *refusal*.** A
  recovery that returned zero, clamped, or widened wrongly passed it, so the
  dispatch tests gained `i32_argument_reaches_the_handler_verbatim`, which
  asserts the value the mock handler receives.

### Done — the three duplications, and the shape a total narrowing takes

**The four field sinks are one.** `ProcFieldSink`, `PparentFieldSink`,
`CommFieldSink`, and `StartFieldSink` in `kernel/syscall/src/table.rs` differed
only in the key they watched, and the module's own `RecordingSink` already
captured every field; the four tests now read `field_values(key)`, which went
from one caller to five.

That promoted a latent trap in `build_caps`, which cleared the sink's recorded
*events* after the derivation but not its recorded *fields* — and the
derivation emits a `task` field of its own. With one field reader that was
invisible; with five it is one test away from a count that reads as "the
dispatcher emitted two records". `build_caps` now drops the record whole, and
`a_reserved_upper_bit_never_aliases_onto_a_real_syscall` asserts the sink is
empty before the first dispatch (it fails with either clear removed).

**`lib/virtio`'s 64-bit registers have one split.** The §2.2 sibling carve-out
does **not** cover this helper, and the premise recorded here before — that
deduplicating it needs a shared "has a `write_u32`" seam — was wrong. Both
transports write through the *same* type: `tairix_abi::RegisterWindow`
(`MmioTransport::window`, `PciTransport::windows.common`), calling the same
`write_u32`, mapping the same `WindowError` onto the same
`VirtioError::DeviceFault`, splitting the value with the same two lines. The
seam already existed as a concrete type, so no trait and no generic was needed.
What *is* sibling — the register maps (`regs::QUEUE_DESC_LOW` against
`common::QUEUE_DESC`), the reset handshakes, the notify schemes, MSI-X — is
untouched; the transports were not collapsed.

Three `pub(crate)` items in `transport.rs` hold it: `le_halves` (the split,
three callers), `u64_from_le_halves` (its inverse, which `device_features`
open-coded on both sides), and `write_u64_halves` over a `&RegisterWindow` (the
low-then-high pair write, two callers). Six copies and four
`#[allow(clippy::cast_possible_truncation)]` are gone.

It stays in `lib/virtio`, not on `RegisterWindow` in `lib/abi`: the window has
no `u64` accessor because virtio *defines* its 64-bit registers as two 32-bit
accesses, and a `write_u64` on the generic window would advertise a single
access it does not perform. It is not hoisted tree-wide either — the twelve
other `>> 32) as u32` sites are unrelated operations (an MSI-X address half, a
PRNG word, an x86 `wrmsr` `edx` operand, a FAT cursor), and a shared helper
over those would be the one-liner-wrapping module §2.3 forbids.

Both `queue_set` tests programmed `avail` and `used` with values under 2^32, so
their high halves were zero *and never asserted*: a transport that wrote only
the low half of those two ring addresses passed. Demonstrated, not assumed —
that mutation passes the pre-change assertions and fails the widened ones. All
six halves are now distinct and asserted.

**A total narrowing is written so it is total.** Neither shape the entry
recorded was right; the third one is. `(x & 0xFFFF_FFFF) as u32`,
`(x >> 32) as u32`, and `((x & MASK) >> SHIFT) as u8` are all
`cast_possible_truncation`-clean — clippy reduces a masked or shifted operand's
width before comparing it to the target — so the narrowing needs neither a
suppression to justify nor an error arm that cannot be taken. Five sites in
`lib/abi/src/hwtree.rs`:

- `framebuffer_mode`'s height decode dropped
  `.map_err(|_| Errno::LengthOutOfRange)` (unreachable), and its width decode
  dropped the suppression.
- `framebuffer_mode`'s format decode dropped a suppression that was silencing
  nothing — the mask is `0xFF`. Same false-justification class as the two this
  sweep already found.
- `framebuffer_memory` masks the field before shifting it down. **No test can
  distinguish this**: the reserved-bit check six lines above already bounds
  `flags` to sixteen bits, so both forms agree on every reachable input. It is
  a suppression removal, not a behaviour fix — made because the suppression's
  comment credited the shift for a bound the *guard* supplied.
- `link_address_octets` dropped `u8::try_from(…).unwrap_or(0)` — unreachable,
  and its recovery silently substitutes a zero octet. Both it and
  `link_address` are now `to_le_bytes`/`from_le_bytes` over the low six bytes,
  so the pair is visibly inverse rather than a hand-rolled shift loop against a
  mask.

Regression cover for the two decodes, since the removed arms were unreachable
and no test could fail *before*:
`framebuffer_mode_carries_both_geometry_halves_whole` drives the widest geometry
the constructor admits (`0x3FFF_FFFF` × `0xFFFF_FFFF` — a `u32` stride caps the
width at four bytes per pixel), and
`link_address_octets_reads_only_the_six_it_packed` drives distinct octets plus a
wire resource carrying junk in the two `base` bytes above the address. Every
other fixture in that module stays inside sixteen bits, so a decode narrowed to
those passed before and fails now. Probed by mutation: narrowing either half,
reading one half for the other, and shifting the octet window each fail.

### Done — no `.rs` comment cites a charter section, and `cargo xtask charter-cite` keeps it that way

614 citations over 539 lines in 143 files are gone: each became the prose
reason already standing beside it ("fail closed", "one definition", "parallel
implementations"), or was deleted where the sentence already carried the reason.
The count is higher than the 278 first recorded because the original scan
suppressed a whole comment *block* when any legitimate reference appeared in it,
so a bare `(§2.2)` sharing a block with `plans/NETWORK.md §5` was invisible.

Removal is paragraph-scoped, not line-scoped: a citation inside a wrapped
parenthetical cannot be deleted line by line without orphaning punctuation
(`/// .`) or destroying a markdown bullet's continuation indent, so each
affected paragraph was joined, rewritten, and re-wrapped at 80 columns with
code spans and intra-doc links protected from being split. Both invariants were
then machine-checked against `HEAD`: every code span and `[link]` survives
verbatim, and 140 of the 167 touched files differ in *prose* only by the removed
citations — the other 27 carry the deliberate substitutions below.

Substitutions worth knowing about, because a future reader may wonder where a
number went:

- `§5.3-checked` → `permission-checked` (six sites in the file manager and
  `lib/browse`), `under §26 load` → `under contended multi-user load`,
  `the signed §8 load gate` → `the signed driver load gate`,
  `the §16.2 / §16.3 mount policy` → `their mount policy`.
- A bare `§N` that meant a *sibling* document rather than the charter was
  **anchored**, not deleted: `spec §13` in `lib/controls` (its module doc names
  `plans/GUI-CONTROLS-DESIGN.md`), `RFC 1184 §2` in `telnet`, `RFC 8415 §18.2`
  in the DHCPv6 engines, `plans/APPS.md §2.1` in `lib/help`'s lint. Otherwise
  the next scan re-flags them and someone deletes a real reference. Seven that
  the automatic pass had deleted before this was understood were restored
  anchored (four `spec §13`/`§15` in `lib/controls`, three `SYSLOG §5.1` in
  `lib/log/src/segment.rs`) — a crate whose sibling document numbers its
  sections the way the charter does is where this class hides.

Five defects surfaced while sweeping, each fixed here:

- **Ten dangling comments from an earlier half-removal.** Six QEMU fixtures
  carried `// — test affordances must never reach a release binary.` — the
  subject (`AGENTS.md §1`) had been stripped, leaving a sentence starting with
  an em-dash. Same shape in `unlock_service.rs`, `acpi.rs`, `gdt.rs`, and
  `x86_64/boot.rs` (which had lost a word mid-phrase: "sized from the
  -discovered CPU count").
- **`(§ relocation defence)`** in `kernel/mem/src/swap/tests.rs` — a section
  sign attached to nothing, the residue of a partial edit. Three more in
  `drivers/filesystem/arxfs/src/tests.rs` had lost a list member and kept its
  separator (`§15.12, §16;)`).
- **Two identifier code spans broken mid-token** by the original wrapping, so
  the documented command did not work: ``` `cargo build … -p tairix-test-
  syscall-dispatch-qemu` ``` and `DeviceManager:: autoload`. Markdown renders a
  span's newline as a space, so these were already wrong on the rendered page;
  joining the paragraph made them visible.
- **`(§2.4)` in `lib/net/src/bond.rs`** and **`(§2.6.5)` in
  `kernel/core/src/syscalls.rs`** — the first a charter citation the scan had
  suppressed because `802.3ad` sat beside it, the second a `plans/FIX-DESKTOP.md`
  reference that named no document.

The guard is `cargo xtask charter-cite`, a static gate in `ci` beside
`spec-review`. It scans every file type that has a comment (see the entry
below); over each, two rules: a comment must not name the
charter file beside a section reference (naming it in prose is fine — the
charter asks for that), and a `§N` whose number is one of the charter's own
section labels must have its source named within the same comment paragraph, so
`RFC 9293 §3.2` and `` `plans/APPS.md` §4 `` pass while `(§2.2)` does not. The
label set is *derived* from `AGENTS.md` at check time — its headings, the
numbers it cites, and the ordered-list items each heading introduces (the
charter numbers its rules as list items, so `§2.24` exists in no heading) — and
never copied, so a new rule needs no edit here.

Three things about the checker a later reviewer should not re-litigate:

- **It lexes rather than pattern-matches.** A section number in a string
  literal is program output, which the charter permits — a `compile_error!`
  message, or the provenance banner `font-atlas` stamps into its generated
  file. Those literals span lines, so the scan carries `Lex` state across them;
  a line-local `//` search reads the banner's continuations as comments and
  destroys the one citation the charter sanctions.
- **The anchor window is one clause (45 characters), not the paragraph.** A
  paragraph-wide anchor is what let the original scan miss `(§2.2)` beside a
  legitimate reference. The fifteen references this tightening flagged were
  genuinely ambiguous — a `§18.2` sixty characters from its `RFC 8415`, a
  `§11.4` in a `lib/controls` banner, a `§7.3` in `lib/log` — and are now
  anchored to the document they meant.
- **A generated file is skipped, by its own first-line banner.** The charter's
  one sanctioned citation is the provenance a generator stamps onto what it
  emits, and `lib/font/src/atlas.rs` carries exactly that. Editing it to please
  the checker makes the generated view drift from its generator — `font-atlas`
  caught precisely that during this change, which is the whole-project gate
  doing its job. The banner must be the *first* line and a plain `//` comment,
  so a hand-written generator whose module doc mentions the banner it writes
  stays inside the scan.
- **The source vocabulary is evidence, not a wish list.** Every one of the 41
  entries is cited by section somewhere in the tree; a speculative entry is
  both bloat and a hole, because a bare `(§2.2)` passes wherever the word sits
  nearby. Auditing which entry anchored each accepted reference found exactly
  that: `PCI` was the sole anchor of five surviving `(§2.2)` citations in
  `kernel/tairix-kernel/src/hwdiscovery.rs`, whose paragraphs discuss the PCI
  bus. Those are fixed, and 36 unused entries are gone. A newly-cited
  specification adds its name in the change that cites it — the diagnostic says
  so. The audit is the standing mitigation for this class and is repeated, not
  assumed: the widening below found five more accidental anchors in `.rs` that
  this pass had missed.
- **`§X`, `§SYSRET`, and `§"Overflow"` are accepted.** A section named rather
  than numbered cannot be a charter citation; only a sign attached to nothing
  is refused.

### Done — a comment is a comment whatever the file spells it with

`charter-cite` now scans every tracked file type that *has* a comment — Rust,
the assembly stubs, `Cargo.toml` and its siblings, the CI shell scripts, and
the workflow YAML — and 1036 citations across 300 files are gone. Only the
extension filter, the comment-marker set, and the third rule below changed; the
label derivation, the source vocabulary, and the anchoring test are the same
ones the `.rs` sweep used.

`README.md`, `docs/src/**`, and `plans/*.md` stay outside it, and now by
construction rather than by an exclusion list: they map to no comment syntax.
They are prose documents that cite the rules they explain or implement, which
is the cross-reference the charter asks for.

**A `Cargo.toml` `description` is scanned, as a third rule.** It is a value
rather than a comment, so the decision needed making rather than assuming: it
is scanned because it is the crate's own prose about why it exists — the same
job its `//!` module doc does, which the `.rs` sweep already cleaned — and it is
read through `cargo metadata` and the generated SBOM, away from the charter,
where a bare section number resolves to nothing at all. 148 of the 173
`description` citations were refused (the rest name a plan or a spec beside the
number); the report names the surface, so a `description` finding is not read as
a comment finding.

Four things about the widening a later reviewer should not re-litigate:

- **`#` is not one marker.** TOML spells a comment with `#` outside a string,
  unconditionally. Shell and YAML need it at the start of a word, or
  `${#list[@]}` and `"$#"` read as comments. The assembler needs *whitespace
  after* it, which is what tells a comment marker from the AArch64 immediate
  prefix that binds to its value with none (`mov x5, #0xffff`) — and `//` plus
  `/* … */` are comments on every target the integrated assembler serves, so
  aarch64 and x86_64 headers need no target knowledge. Each of the three rules
  is probed by a test that fails when it is loosened to "`#` anywhere".
- **The lexer had to grow single-quoted and triple-quoted strings.** Rust
  spells a character constant `'c'`, so only the double quote opens a string
  there; TOML, shell, and YAML take both, and TOML's `"""…"""` spans lines. The
  `description` rule needs the triple-quoted form read whole, so it is not
  optional machinery.
- **`.md` is not on the extension list**, so the prose documents are skipped
  without an exclusion path to keep in sync. The test pins that.
- **The generated-file skip is per-syntax.** A generator stamping the governing
  rule onto what it emits writes the banner in that file's own comment
  spelling, so the skip tests `#` for a manifest and `//` for Rust.

The 1036 removals are the same shape the `.rs` sweep found: a trailing
parenthetical whose content is nothing but citations (deleted whole, because the
prose beside it already carried the reason — "one ABI definition", "never
re-implemented", "the same premultiplied-alpha primitives"), or a citation
introducing its reason after an em dash (`(AGENTS.md §1 — no hacks; §5.4.5 —
fail closed)` → `(no hacks; fail closed)`). Six kinds of collateral damage were
found by auditing the diff rather than by the gate, and each is worth knowing
about before any similar sweep:

- **A citation can be the sentence's subject or object.** `§17.4 permits
  `kernel/mem` to name `kernel/arch/api`` and `is permitted by §17.4` leave a
  verb with nothing attached; `the belt-and-braces fallback `AGENTS.md` §2.9
  requires` leaves a dangling verb. Twenty-three such sites were rewritten in
  prose, not mechanically.
- **A citation run can straddle a line break**, leaving the separator or the em
  dash orphaned on the next line (`(—`, `(/)`, `(fail-loud,)`). Deleting in
  place per line and joining only the smallest straddling run fixes it; the
  first attempt reflowed whole paragraphs and merged a hanging-indent register
  list and an aligned vector-table into prose.
- **A punctuation tidy eats real syntax.** Collapsing `(` + separator ate the
  `-` of `(-> !)`, the `::` of `[::facet]`, the `--` of `[--sequential]`, and
  the leading `/` of `(/System/Services/…)`; dropping an empty `()` ate the call
  parentheses of ``halt()``, ``FnMut()``, ``UserMode::new()``. Fourteen sites,
  all restored, all found by a token-level old-vs-new diff of every changed
  file — which is the audit any such sweep needs, because none of this fails a
  build.
- **Collapsing runs of spaces destroys an aligned table.** `tools/ci/soak.sh`'s
  usage block was the one casualty; restored.

**The false-anchor audit is the real finding.** The source vocabulary anchors a
reference when the source name sits within one clause of the `§`, so a comment
*about* USB, PCI, virtio, DNS, or HID can have a charter citation accepted by
accident. Auditing which entry anchored each of the 121 accepted references in
the newly-scanned files found 20 such citations, and repeating the audit over
`.rs` found 5 more the earlier sweep had left — `§18.6` in
`drivers/bus/usb/vl805` (both the manifest and `src/lib.rs`), `§2.2` twice in
`lib/abi/src/sysinfo.rs` behind a `plans/FIX-IO.md` reference, `§8` in
`lib/virtio_input/src/lib.rs`, `§16.7` in `userland/apps/applib/src/lib.rs`.
All 25 are fixed. The anchoring rule itself is unchanged: tightening it to
"the source must be adjacent" was measured and rejected — it newly flags 17
`.rs` sites of which 13 are *legitimate* references (`HID Usage Tables §3`,
`Arm DEN 0022, §5.2.2`, `RFC 6298 §2.2 vs §2.3`), so it trades a documented
heuristic limit for churn on correct citations. Blanking `AGENTS.md` as an
anchor was also measured and rejected as bloat: it newly flags 679 citations
and every one is already refused by the charter-name rule.

Every remaining charter-shaped `§` in the scanned tree — 92 in the new file
types, all of them — names its document adjacently (`plans/APPS.md §4, §6`,
`docs/src/filesystem/arxfs-spec.md §5, §8`, `tests/SECURITY.md §5`, `SYSLOG
§14`). That list is the audit's output and is what a future reviewer re-derives
rather than trusting.

Nine tests cover the widening, each probed by loosening its guard: the
extension map (and that `.md` is not on it), the manifest string lexer, the
shell word boundary, the assembler immediate prefix, the description rule, the
"only a `description` is prose" restriction, the multi-line description, and the
per-syntax generated-file skip. The whole-tree assertion now covers every
scanned type.

A tenth drives a fixed-seed adversarial corpus — unterminated literals, lone
markers, multibyte boundaries — through all four grammars and the description
reader, because the scan is the first gate in `ci` and a panic there would block
the pipeline on a file it merely could not lex. It proves the lexer is total;
the `checked_sub` on the `#` lookbehind is the one boundary guard that has
independent cover (removing it panics).

### Done — kernel IPC payloads are wiped on release (§4, §23.1)

Every kernel-owned IPC payload copy is now a `tairix_kernel_mem`
`SensitiveBuffer` and is zeroed when the kernel releases it: `PendingCall`,
`ReceivedCall`, the `completed` reply entry, `Message::payload`, and the four
syscall staging buffers (`ipc_send`, `ipc_call`, `call_post`, `call_reply`).
That covers delivery, every refusal, poster withdrawal, deadline reap, poster
exit, and endpoint teardown.

The choice was unconditional wiping, not a per-endpoint "carries-secrets" bit:
an opt-in bit is open-by-default for every endpoint whose author did not
anticipate a secret, and the endpoints that carry one are not knowable at bind
time (session/elevation passphrases, app-data sealed secrets, delegated
capability tokens). Cost is one write pass over bytes the path already copies
at least twice, and the copy moved *out* of the endpoint spinlock, so the wipe
is never paid inside a critical section. Both `post` and `reply` gained an
`Errno::OutOfMemory` path (they previously grew a `Vec`, which aborts on
exhaustion), reported through the new `PAYLOAD_ALLOC_FAILED` (3060) audit
event rather than conflated with a capability denial.

Two things a later reviewer should not re-litigate:

- **`post` allocates before the capacity check**, so a caller spamming a full
  endpoint churns one transient alloc/free per attempt where it previously
  only took the lock. Deliberate: the footprint is O(1) per concurrent caller
  (freed within the call), the full queue is the exceptional path, and
  allocating under a spinlock is the worse defect. The syscall path's
  behaviour is unchanged in kind — it already staged a buffer before `post`.
- **The staging copy is still a second copy.** `post`/`reply`/`send` take
  `&[u8]` and allocate internally, so the syscall layer stages once and the
  endpoint copies again. Threading an owned buffer through would remove one
  alloc and one copy, but it touches ~90 call sites and there is no
  measurement saying the copy matters — that is the blind micro-optimisation
  §2.16 warns against. It is a *measured* optimisation candidate, not a
  defect.

Regression cover is `kernel/ipc/src/payload_wipe_tests.rs`: a test-only global
allocator scans every released block for the payload sentinel, across a served
round trip, the refused paths, the abandoned paths, and a port message. One
case leaks a payload deliberately so the scan cannot pass vacuously; all six
fail with the wipe disabled.

### Done — the staging table is bounded in aggregate, per account, and per process (§24.4, §26.2)

`AppData::sessions` grew one entry per calling process instance, reclaimed only
by age, with `MAX_PENDING_EDITS` bounding edits per session per scope and
nothing bounding the sum. The chosen shape is **nested ceilings with the account
as the fairness unit**: per scope (keys, unchanged), per process instance
(bytes), per account (bytes and entries), and per service (bytes and entries).
The account ceilings are each `1/STAGING_ACCOUNT_SHARES` (16) of the whole and a
process may hold half its account's share, so filling the table takes at least
sixteen distinct accounts and neither one account nor one application can deny
the others their settings. `STAGING_TOTAL_MAX_BYTES` is 8 MiB — under one
percent of the 1 GiB floor, and four orders of magnitude above any real load,
because the client stages locally and only sends its `ConfigSet` calls inside
`commit()`.

The reasoning, the rejected alternatives (LRU eviction, a per-app share), the
fairness cost, and the residual side channel are in `plans/APPDATA.md` §3.8;
the reader-facing table is `docs/src/userland/confd.md`.

Three things a later reviewer should not re-litigate:

- **Charged bytes include the `PendingEdit` record, not just key and value.**
  A thousand one-byte keys cost the table a thousand records; charging only the
  text would have let a caller past every ceiling by some fifty times the
  intended footprint. Allocator slack above the charge is a bounded constant
  factor and deliberately unmodelled.
- **The entry counts are not redundant with the byte ceilings.** A session
  holding one one-byte key costs almost nothing yet is still scanned on every
  request, and the byte ceiling alone admits tens of thousands of them.
- **The ceilings bound memory; they do not predict that a commit will
  succeed.** Whether staged edits fit the document they publish into depends on
  the committed document, which the service has not read at staging time —
  reading it per staged edit would cost a file read each. An over-large set of
  edits is still refused at the commit, by the format.

Regression cover is ten tests in `userland/system/confd/src/tests.rs`, one per
guard, each verified to fail with its guard removed: the aggregate ceiling,
the per-account and per-process byte shares, the two entry counts, the record
charge, `recharge` on stage and on clear, the replaced-edit charge, and
`the_widest_legal_rewrite_of_both_documents_is_admitted`, which enforces that no
legal edit is refused rather than asserting it in prose.

### Done — §23.1 security review of the new app-data subsystem

`lib/appdata`, `userland/system/confd` (`vault`, `store`, `bulk`, `owner`), and
`lib/abi/src/appdata_ipc.rs` were traced entry point by entry point for
capability-check-before-state, per-field validation, fail-closed error paths,
and secret zeroisation on every exit. Two secret-hygiene defects were found and
fixed; the two above were found and escalated.

Fixed:

- **The app-data client left every application's secrets in freed heap.**
  `lib/appdata`'s `negotiate` read a `VaultRead` answer into a transport `Vec`,
  parsed it into a `Document` (which wipes its lines on drop), and dropped the
  `Vec` un-wiped — so every `Vault::open`/`reload` left a full plaintext copy of
  the application's secrets in freed memory that the userland heap deliberately
  does not scrub (§25). The read attempt is now `attempt`, which answers an
  *owned* `Answer` so the buffer's borrow ends before it returns, and wipes it
  on the success and the refusal path alike.
- **`confd` skipped its request wipe on the fail-closed origin path.** The serve
  loop wiped `request[..len]` only after a served request, so the two `continue`
  routes taken when the caller's kernel-attested origin cannot be read left a
  `VaultSet` frame's plaintext in a long-lived buffer that the next request only
  partly overwrites. The wipe now lives in `AppData::serve` (which takes
  `&mut [u8]`, so it covers every host of the engine and is host-testable), and
  `run.rs` has one wipe for the single case the dispatcher never sees the frame.

Confirmed sound, and needing no further pass:

- **The master-secret buffer is wiped by its caller.** `MasterSecret::decode`
  delegates the wipe to the reader, and `AppStore::master` honours it: the `Vec`
  the record was read into is zeroized before the function returns, on the
  accept *and* the refuse path, and `master_or_draw` wipes the encoded record
  once the write has landed. `from_bytes` consumes the caller's array by `&mut`
  so no `Copy` of a secret is left on a caller's stack.
- **Authority precedes state on every operation.** `dispatch` resolves the
  attested identity before the match, so a principal with no verified bundle is
  refused whatever it sent; the store's gated root ownership is re-proved on
  every call even for a cached home (`RootCache::root_of` against
  `CONFD_UID_RAW`, a real `const` and therefore a pattern match rather than a
  catch-all binding).
- **No path composes a caller-supplied path component.** `AppIdentity::new`
  runs `validate_bundle_id`, and `Origin::from_bytes` re-validates the identity
  tail through it, so a bundle id used as a directory name cannot traverse,
  hide, or case-fold; the wire's foreign identifier and every bulk name pass
  the same store-name grammar, re-stated inside `bulk` rather than trusted from
  the decoder.
- **The descriptor delegation cannot land on the wrong task.** `fd_grant`
  resolves the recipient under the same write lock that mints, and scheduler
  task ids come from a monotonic counter, so the kernel-attested `origin.pid()`
  resolves to the intended process or to nothing.

### Open — rustdoc waffle in the new surface (§2.11, `plans/WAFFLE.md`)

Several new modules carry multi-section design essays as module rustdoc
(`userland/system/confd/src/vault.rs` opens with ~52 lines under "# The
hierarchy", "# What protects the master secret, stated plainly", "# What the
sealing does buy"). Mandatory rustdoc is held to the same terseness as any
comment. Coordinate with `plans/WAFFLE.md` rather than opening a second sweep.

### Done — dead code and false-justification lint silencing (§2.14, §15.3, §15.10)

Every `dead_code` allow in the tree was probed by removing it and rebuilding.
Fourteen carried justifications that were simply false — the code they claimed
to protect produced no warning at all — and are gone along with the code that
was genuinely dead. What remains is `cfg_attr`-scoped to one target or to
`loom`, a const-assert, or a shared test fixture compiled into several
binaries, and each states the real reason.

Structural findings worth keeping in mind while auditing the rest:

- An inherent method that duplicates a trait default silently wins method
  resolution, so tests calling it through the concrete type never reach the
  trait default the production path uses. `lib/pci`'s `Pci::map_virtio_window`
  duplicated `VirtioPciBus::map_virtio_window` exactly, and every test went to
  the copy. Trait-forward seams are now covered through `&dyn` in that crate;
  the same shape is worth checking wherever a crate forwards inherent methods
  into a trait impl.
- `fn _suppress_no_main() {}` — an empty function whose only purpose was to
  silence a lint, itself silenced by `#[allow(dead_code)]` — had been
  replicated into 68 integration-test programs and one kernel binary. None of
  them suppressed anything.

### Done — one decode from a raw result to an `Errno`, and it is total

`lib/rt::errno_from_raw` was not the seam; it was a **second** definition of
one `lib/abi` already owned and documented as "the one definition of that
decode, so no caller re-derives it" (`Errno::from_syscall`, ~350 callers
against `errno_from_raw`'s handful). The two disagreed on exactly one input,
which is the divergence a second definition always eventually buys.

`Errno::from_syscall` **aborted the process on `i64::MIN`**. It spelled the
recovery `i32::try_from(-ret)`, and the workspace sets `overflow-checks = true`
with `panic = "abort"` in *both* the `release` and `dev` profiles, so negating
the one value that cannot be negated killed the program — in the most-called
conversion in the tree, on a register the caller had already classified as an
error (`ret < 0` includes `i64::MIN`). A function total over every other input
(unknown code, out-of-`i32` magnitude, non-negative value all fail closed to
`NotImplemented`) took one input to an abort. `errno_from_raw` guarded it; the
seam did not.

The shape now is a fallible primitive plus its fail-closed binding, both on
`Errno` in `lib/abi` — the bottom of the layering graph, so nothing links a
runtime to decode a register:

- `try_from_syscall(i64) -> Option<Errno>` — `checked_neg`, then `i32`, then
  the discriminant. `None` is every unreadable register alike.
- `from_syscall(i64) -> Errno` — that, `unwrap_or(NotImplemented)`.
- `try_from_status(i32) -> Option<Errno>` — the wire-status entry point (below),
  delegating, so there is still exactly one decode.

`errno_from_raw` is deleted, not deprecated, and its tests moved to `lib/abi`
rather than being duplicated there. **Twenty-two private re-implementations are
gone**, across `lib/{blkclient,display,font,netchan,sandbox,drvrt}`,
`lib/rt/src/{thread,sync}.rs`, `drivers/{display/framebuffer,storage/volmgr,
storage/usb_msd,input/usb_kbd,input/usb_mouse,bus/usb/xhci}`,
`userland/apps/{files,terminal,viewer,wallpaper,widgets,datetime}`,
`userland/{net/netstack,session/login,shell/elsh,shell/users}`,
`userland/system/{confd,devmgr,init,journald,seatmgr,sysinfod}`, and the
`blkio_fault_program` / `sandbox_program` fixtures. `drivers/network/
virtio_net_driver` was on the recorded list but has no `Errno` at all.

**Three of the four divergent fallbacks were defects, not preferences.** Each
site's fallback was read and each caller traced before it was replaced:

- **`NotFound` (six sites) fabricated "the device was unplugged".** In
  `usb_msd` it set `disconnected`, which retracts the LUN nodes and exits for a
  reload; in `usb_kbd`/`usb_mouse` it became `DriverError::NotFound`, which the
  pump loop logs as "transport disappeared" and exits `0` on. So a refusal the
  build merely could not *read* tore the device down and reported a clean
  unplug. It now reads as `NotImplemented`, the drivers report the concrete
  failure, and the bounded consecutive-error ladder fails it closed — no spin
  (the loops park on their wait-sets), and the diagnosis is stated rather than
  invented.
- **`DeviceFault` (`lib/netchan`, `lib/sandbox`) and `OutOfRange`
  (`lib/rt/src/{thread,sync}.rs`)** each asserted a specific cause for a value
  with no readable cause. No caller branched on either (the live branches are
  `WouldBlock`, `AlreadyExists`, `TimedOut`, `Interrupted`, `PermissionDenied`),
  so the change is observable only on a code no working kernel returns.
- **`NotImplemented` (the rest)** was already the seam's answer.

**`lib/drvrt::decode_errno` kept its `Option` shape, deliberately.** Folding it
onto `from_syscall` would have been a regression: `msi_alloc_error` maps
`NotImplemented` to "no MSI controller on this platform", so an unreadable code
would have become a *confident claim about the hardware*. It is now
`Errno::try_from_syscall` — the seam's fallible form — and its seven callers
keep choosing their own fold. That consumer is why the primitive is fallible
and the total form is derived, rather than the reverse.

### Done — the same defect in the wire-status decoders (§23.1)

Found by scanning what remained: nine `lib/abi` decoders recover an `Errno`
from a **reply frame's** negative `i32` status word, and **three negated it
unguarded** — `log_ingress::decode_reply`, `mailbox_ipc::decode_reply`, and
`driver_store::reply_status`. A status word comes from a *peer process*, not the
kernel, so `i32::MIN` in a reply frame aborted the decoding process: a
remote-triggerable kill, and worse than the syscall-register case. Six sibling
decoders already had `checked_neg` — `usb_urb` even documents why — which is the
empirical case for one definition rather than nine chances to forget.

All nine now go through `Errno::try_from_status`, which delegates to
`try_from_syscall` (widening an `i32` makes the negation unconditionally safe).
The fail-closed answer stays each protocol's: `BadMagic` for the wire formats
that call a corrupt frame that, `OutOfRange` for the others. The three fixes
each carry a test that aborts without the guard.

Two things a later reviewer should not re-litigate:

- **`try_from_status` is not a wrapper worth deleting.** It is one delegating
  line, but it is the typed entry point for the wire family: it names the
  distinction (a peer's status word is not a kernel result), it makes
  `i64::from(status)` impossible to forget, and it has ten callers. Three
  sites got the raw spelling wrong; the fourth kind of mistake is what a name
  prevents.
- **Three `lib/abi` sites that look similar are not this family.**
  `blkio::decode_completion`, `field.rs`'s `ScalarType::Error`, and
  `sysinfo.rs`'s reply status read an *unnegated* code, so they never negate and
  need no guard. The four surviving `(-err.as_i32())` sites are **encoders** over
  a small positive discriminant.

### Done — `lib/hid` owns the boot-protocol pump's error policy (§2.2)

Surfaced by the fallback work: `transport_error` and `pump_error_limit_reached`
were byte-identical in `usb_kbd` and `usb_mouse`, and writing the same
regression test twice was the signal. Both now live in `lib/hid`, beside the
`pump_once` loop they serve, with the drivers' local copies and their
now-redundant test modules deleted. The §2.2 sibling carve-out does not reach
this: two identical helper functions are not parallel implementations of a
trait.

The tests moved up with them and gained the case neither driver had: an
unreadable refusal (including `i64::MIN`) must not classify as a removed
device. Probed both ways — restoring the drivers' old inline decode fails it on
the abort, and restoring just the `NotFound` fallback fails it on the
misattribution.

### Note for the next context — a text sweep needs its own audit

Nothing in the validation gate can tell you a comment sweep broke a sentence:
`cargo check`, clippy, and `docs-check` are all silent on prose. The audits that
*did* find the damage in the widening above, and that any similar sweep should
run before the gate, are all cheap:

1. **A token-level old-vs-new diff of every changed file**, with citation runs
   and the charter name stripped from both sides. Anything left over is either a
   deliberate rewrite or collateral damage — 14 eaten `()`/`::`/`--`/`/` tokens
   surfaced this way and by nothing else.
2. **A survival check for every external reference.** For each `<source> §N` in
   `HEAD` whose source is not the charter, assert it survives verbatim. Five
   legitimate references had been deleted because the charter was named in the
   same clause and the paragraph-level charter-name rule condemns everything in
   its window; the rewriter must remove the charter citation *first* and
   re-evaluate, not process a paragraph's citations in one pass.
3. **A prose-shape scan of every changed paragraph** for a citation that was the
   sentence's subject or object (`§17.4 permits …`, `… is permitted by §17.4`,
   `the fallback §2.9 requires`), an orphaned separator or em dash at a line
   edge, an empty parenthetical, and collapsed column alignment.
4. **`cargo metadata --locked`, `bash -n`, and a YAML parse** over the changed
   manifests, scripts, and workflows — a comment sweep that corrupts one of
   those is otherwise found only much later.

### Note for the next context — the fixture check earns its keep

The recorded trap is real and it fired here. `userland/system/confd`'s `Run`
program is `cfg(freestanding)` and is built by **`kernel/tairix-kernel`'s build
script**, so `cargo build --workspace`, `cargo test --workspace`, and host
clippy all pass while it is broken. A rewrite of `errno(x)` call sites missed
the bare `map_err(errno)` references, and the only thing that saw it was
`cargo check --locked --target <t> -p …` over the 149 enrolled pairs. Run that
before the gate whenever `lib/abi` changes — every fixture links it.
