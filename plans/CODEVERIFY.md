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

### Open — unjustified `#[allow(...)]`, 168 sites tree-wide (§15.10)

An `#[allow]` with no comment tying it to a real invariant. Distribution:
`tests/integration` 62, `kernel/arch` 16, `kernel/sched` 12, `userland/apps`
11, `lib/virtio` 10, `userland/gui` 8, `lib/abi` 8, `kernel/tairix-kernel` 8,
`kernel/syscall` 8, `kernel/core` 4, remainder in `lib/*`. Mostly
`clippy::cast_*`. Each is either a lossless conversion whose invariant should
be stated, or a cast that should be a checked conversion instead — decide per
site; do not blanket-annotate. The `dead_code` allows are already resolved (see
below) and are not part of this count.

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

### Open — the staging table has a per-session bound but no aggregate one (§24.4, §26.2)

`AppData::sessions` grows one entry per calling process instance and is
reclaimed only by age (`STAGING_IDLE_NS`, 60 s). `MAX_PENDING_EDITS`
(= `MAX_SETTINGS`, 512) bounds edits per session **per scope**, so one session
caps at ~1.2 MiB of staged keys and values — but nothing bounds the sum across
sessions. Staged bytes therefore scale with the `ConfigSet` calls any
application of any account can issue inside the reclaim window, with no
ceiling, in a boot-floor service every command app depends on. On the §26.7
floor (1 GiB) that is a denial of service against every application's settings.
`plans/APPDATA.md` reasons about the per-session and per-scope bounds and about
age reclaim, and does not address the aggregate.

Escalated rather than fixed here because the containment bound's *shape* is a
fairness decision, not a constant: an aggregate edit ceiling lets one
application starve another's staging (§26.2), a session-count ceiling has the
same problem one level up, and LRU eviction bounds memory without refusing
anyone but silently discards a settings sheet's unsaved edits. Pick the
trade-off, then land it fail-closed with its test.

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
