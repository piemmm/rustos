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
- **The gate lints bare-metal code, so the lint-catchable classes need no
  manual hunt.** `cargo xtask clippy` runs `-D warnings` for the host *and*
  once per Tier-1 target, so a `freestanding` body is linted rather than only
  compiled (`docs/src/contributing.md`). Scan for what a lint cannot see —
  wrong invariants, ambient authority, duplication, dead code — not for what
  the gate already rejects.

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

### Open — fixture exit-code reporting is duplicated sixteen ways, untested

The `u16::try_from(code).unwrap_or(u16::MAX)` + `saturating_add` conversion
above is now correct but written out at sixteen sites, and it has no host test:
the fixture bodies compile only under `freestanding`, and
`tests/integration/harness` is a **build**-dependency, so nothing a bare-metal
fixture links at runtime can host the shared version. A `lib/*` crate for
test-fixture arithmetic would be bloat (§2.3) and speculative production
surface (§2.4), so the honest fix is a runtime fixture-support seam the
bare-metal binaries can depend on — worth a context of its own, not a
smuggled-in extra.

### Open — three duplications noticed while sweeping the `#[allow]` sites

Recorded rather than folded into that change (§2.18), each small enough for one
context:

- **Four near-identical field-capturing sinks** in `kernel/syscall/src/table.rs`
  tests (`ProcFieldSink`, `PparentFieldSink`, `CommFieldSink`,
  `StartFieldSink`), each differing only in the key it watches. The shared
  `RecordingSink` in the same module now captures every field and answers
  `field_values(key)`, so all four collapse into it.
- **`write_u64` / `write_u64_pair`** in `lib/virtio`'s PCI and MMIO transports
  are the same algorithm over a different window type. They are sibling
  `Transport` implementations, so the §2.2 carve-out arguably covers them —
  but the *helper* is not the sibling behaviour, and deduplicating it needs a
  shared "has a `write_u32`" seam. Decide deliberately; do not collapse the
  transports themselves.
- **A dead error path in `lib/abi/src/hwtree.rs`**:
  `u32::try_from(self.xlate >> 32).map_err(|_| Errno::LengthOutOfRange)?`
  cannot fail — shifting a `u64` right by 32 leaves at most 32 bits — so the
  `LengthOutOfRange` arm is unreachable. The sibling width decode above it is a
  stated-invariant cast. One of the two shapes is right for both; pick it.

### Open — charter section numbers cited in comments, 278 lines in 98 files (§2.11)

`AGENTS.md` §N in a comment restates *what* a rule is, never *why* the code
does what it does, and §2.11/§15.17 forbid it outright — including a bare
trailing `(§5.4)`. Rewrite each as the prose reason ("fail closed", "zeroed on
drop") or delete it. References to *other* files — `plans/*.md`,
`tests/SECURITY.md`, `docs/src/**`, an external spec, an RFC, a hardware
manual — are legitimate and stay. The one sanctioned exception is a generator
stamping provenance onto a generated artefact.

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

## Open sweep: the raw-syscall-result → `Errno` conversion

`lib/rt::errno_from_raw` is now the one public, tested conversion from a raw
`i64` syscall result to an `Errno`, and the four sites that motivated it (the
private copy in `lib/rt`, and the ones in `drivers/storage/volmgr`,
`drivers/storage/raid_member`, `drivers/storage/raid`) use it.

Roughly nineteen further private re-implementations remain, and they do **not**
agree: three different fallbacks are in use for an unrecognised value
(`NotImplemented`, `NotFound`, and `DeviceFault`), and none guards `i64::MIN`,
whose negation overflows. A caller therefore gets a different error class for
the same kernel refusal depending on which crate it happens to be in — the
divergence duplication invites. Known sites: `drivers/display/framebuffer`,
`drivers/network/virtio_net_driver`, `lib/blkclient`, `lib/display`,
`lib/font`, `lib/sandbox`, `tests/integration/blkio_fault_program`,
`userland/apps/{files,terminal,viewer,widgets}`, `userland/net/netstack`,
`userland/session/login`, `userland/shell/elsh`, and
`userland/system/{devmgr,init,journald,seatmgr,sysinfod}`.

The sweep is its own change: it spans a dozen crates, and each site's current
fallback must be read before it is replaced, because adopting the shared
`NotImplemented` where a caller today branches on `NotFound` would change that
caller's behaviour rather than merely deduplicate it.
