# FIX-WILD — Debuggable user-fault kills (identity, cause, backtrace)

Status: **Stage 1 done; Stage 2 prerequisite done; Stage 1 doc page done;
Stage 2 wiring + Stage 3 planned.** The user-fault containment path is
already correct (it isolates the crash to one task, classifies the address
without leaking layout, audits it with a stable id, records a `139` `wait`
status, and reclaims resources), and its Stage-1 diagnostics are now
documented in `docs/src/architecture/fault-diagnostics.md`. This plan makes
that *correct* path **debuggable**, without adding a single cycle to any
running program's hot path.

Stage 2/3 wiring — validated integration map (design, not yet coded):

- **The register snapshot must come from the faulting *user* frame, not
  `CpuStateCapture::capture()`** (which snapshots *kernel* state). Each arch
  trap handler already holds the interrupted user register state; the
  user-fault resolver ABI (`UserFaultResolveFn` = `fn(fault_va, write)
  -> bool` on each port, plus `DispatchHook::resolve_user_fault`,
  `UserFaultOutcome`, `resolve_user_fault_via_slot`, and the ~7 integration
  `tests/integration/*/kernel.rs` fault callbacks) must thread an
  arch-neutral, self-describing `UserRegisterFrame` (pc/sp/fp + GP set + the
  arch `FrameLayout` + an honest `fp_valid`). This ABI change is **atomic**:
  every port and test kernel changes together or the 4-target build breaks,
  so it is landed in one change, never partially.
- **Per-arch frame availability:** aarch64's EL1 vector (`vectors.s`) already
  saves the whole user frame (x0–x30 incl. x29=fp, ELR=pc, SP_EL0=sp) at the
  base the trap handler holds — no assembly change, fp-backtrace lights up
  immediately. riscv64's `trap::TrapFrame` was extended to save the
  callee-saved set too (`s0`/x8 = fp included, offset asserts updated), so its
  fp-backtrace is live as well. x86_64 needs the same check on its interrupt
  stub.
- **User-stack walk** reuses the shared `tairix_arch_api::backtrace::walk`
  (never a copy) over a `copy_in`-backed fallible `StackReader`; kernel-core
  reaches the faulting task's space+physmap via
  `self.aspaces.read().resolve(task.0)` and its user-stack bounds via
  `aspaces.stack_span(task)`.
- **Load-relative offsets** need the PIE load base recorded per task — this
  plumbing does **not** exist yet in `AddressSpaceRegistry`/captable and is
  part of Stage 2.
- **Crash record** is a bounded, `ProcId`-keyed kernel store, exposed by a
  new versioned/hashed sysinfo query (`SysinfoQueryId::CRASH_RECORD`, next id
  20 + a new `IntrospectDomain` + record types, C-headers regenerated) gated
  on an existing `CAP_SYSINFO_*` member, served via `IntrospectSource` and
  the `sysinfod` broker.
- **Stage 3** threads the fault cause class into the exit record
  (`ProcessWait::record_exit`/`WaitedChild`/`WaitStatus`) so the reaping
  session gets it with the `139` status and writes the `stderr` breadcrumb;
  `WaitStatus::Exited(139)` already encodes SIGSEGV, so a signal-level
  breadcrumb needs no ABI change, but the cause-class refinement does.

Stage 2 implementation notes — validated against the code (coded next):

- **`UserRegisterFrame` home + shape.** It lives in
  `tairix_arch_api::backtrace` (beside `RegisterSnapshot`/`FrameLayout`),
  carrying `{ snapshot: RegisterSnapshot, layout: FrameLayout, fp_valid:
  bool }`, and is threaded by `*const` through the resolver ABI
  (`extern "C" fn(fault_va, write, *const UserRegisterFrame) -> bool` on
  each port; `dispatch_core::resolve_user_fault_via_slot` narrows the raw
  pointer to `Option<&_>`; `DispatchHook::resolve_user_fault` and
  `KernelDispatchHook` take `Option<&UserRegisterFrame>`). Each arch builds
  it from its saved trap frame reusing its own `Backtracer::LAYOUT`.
- **Per-arch saved frame the trap handler builds it from.**
  - *aarch64:* `vectors.s` already saves the whole EL0 frame — frame index
    `x29`=fp @ 29, `ELR_EL1`=pc @ 31, `SP_EL0`=sp @ 33 (byte 264, two words
    past `ELR_FRAME_INDEX`). No assembly change; `fp_valid = true`.
  - *riscv64:* `trap::TrapFrame` saves `ra`/`t*`/`a*` + `s0`..`s11` +
    `sepc`/`sstatus`/`user_sp` — the full-GPR save on trap entry Linux's
    `pt_regs` does, pinned by the `offset_of!` asserts against `trap.s`. `s0`
    is the fp, `sepc` the pc, `user_sp` the sp; `fp_valid = true`.
  - *x86_64:* the `#PF` stub calls `tairix_arch_x86_64_page_fault_dispatch(
    err, cr2, rip, rip_slot)` — it must also pass `*const
    interrupts::SavedRegs` (r8, = `%rsp` before the `subq $8`, giving `rbp`
    =fp and the full GPR set) and the user `rsp` (r9, from the CPU iret
    frame at `152(%rsp)`); `rip` is the pc, `fp_valid = true`.
- **PIE load base source.** `kernel_mem::spawn::build_process_image`
  relocates each segment via `segment.relocated_vaddr(bias)`; the load base
  is the lowest relocated segment vaddr. Return it in `ProcessImage`, thread
  it through `BuiltImage` and the three `spawn_producer.rs` arch builders (the
  only production callers), and record it per task with a new
  `AddressSpaceRegistry::set_load_base`/`load_base`. Frame `pc`s and the
  backtrace resolve to `pc - load_base` for offline `addr2line`.
- **The ABI change and the crash-record consumer are one atomic increment.**
  `record_fault_exit` gains the `regs` parameter, and a parameter that is
  not fully consumed is a defect — so the register snapshot, the
  `copy_in`-backed user-stack walk, the bounded `ProcId`-keyed crash store,
  and its `SysinfoQueryId::CRASH_RECORD` query + `IntrospectSource`/`sysinfod`
  serving must all land in the *same* change (there is no smaller buildable
  slice). Every port + the ~7 `tests/integration/*/kernel.rs` fault
  callbacks change together (atomic 4-target build).
- **Register-dump leak decision (resolving the two policy statements
  below).** In the capability-gated crash record: the `pc`, every backtrace
  frame, and the fault address are **load-/region-relative offsets, never
  absolute** (no ASLR oracle). The raw **GP register *values*** are the one
  datum carried absolute, and only there — a privileged debugger's dump
  gated behind an existing `CAP_SYSINFO_*` member (`CAP_SYSINFO_KERNEL`),
  matching Linux's privilege-gated oops. The shared audit log still carries
  only the coarse non-leaking descriptors (Stage 1). Confirm this split with
  the maintainer before coding if a stricter no-absolute-register posture is
  wanted.

Stage 2 prerequisite landed: the one shared arch-neutral stack unwinder
(`tairix_arch_api::backtrace::walk`, reused by the kernel-panic path and the
future user-fault path — §2.2/§2.21) now reads memory through a **fallible**
`StackReader` (`read_word(addr) -> Option<u64>`). The panic reader over the
kernel's own trusted, in-bounds stack always returns `Some`; the user-fault
reader over the crashing task's **untrusted** stack copies each word in
through the capability-checked user-access path and returns `None` when the
copy faults, so `walk` ends the walk cleanly and the kernel never takes a
fault inside the fault handler. The adversarial fuzz harness
(`kernel/arch/api/tests/fuzz_backtrace.rs`) was extended to also drive the
`None`-terminated path, proving the two invariants (always terminates, never
reads out of the bounds it was given) hold when reads can fail. This is the
safety linchpin the user-stack walk depends on; it is complete, host-tested,
and fuzzed.

Stage 1 landed: `AuditEvent::TaskFaultKilled` (id 4034) now carries the
kernel-attested `name` + `proc_id`, the `write` (store vs load) flag, and a
coarse, **non-leaking** `fault_offset` bucket (`null_page` /
`below_stack_guard` / `region` / `wild`) with a region-relative
`region_offset` distance — never a raw user address. The leak-policy
classification is the one `AddressSpaceRegistry::classify_fault_locality`
definition (`FaultLocality`, `kernel/core/src/aspace.rs`), unit-tested for
every bucket; the fault path threads `write` from `resolve_user_fault` and
does the whole classification allocation-free on the dying task.

Binding under `AGENTS.md`. Nothing here fixes a defect in the kernel; it
enriches the diagnostics emitted when a user task is killed by an
unresolvable fault. The motivating log pair:

```
[320.335] [WARN] id=4034 task killed by unresolvable user fault task=12 fault_class=wild
[320.349] [INFO] id=10004 session ended task=7 user=root uid=1000 session=text exit_code=139
```

`task=12` (a reusable scheduler id) and `fault_class=wild` (outside every
mapping the task owns) is all a developer gets. That is enough to know a
program segfaulted, but not *which* program, *why* (read vs write, near
null vs genuinely wild), or *where* (a backtrace).

Read first (§15.18): **`plans/FIX-PANICS.md`** (the `tairix_arch_api::backtrace`
slice, the arch-neutral bounded fp-walk, and the address-leak policy this
plan extends — the two walkers stay **one shared definition**, §2.2/§2.21),
`plans/WATCHDOG.md` (per-CPU `CpuState` / stack-bounds machinery the walker
sources bounds from), `plans/FIX-PROTECTION.md` (the per-arch
protection-fault fix-up and canary/tag path a fault arrives through),
`docs/src/architecture/panic-diagnostics.md` (the sibling doc page this one
mirrors), and the current fault path:
`kernel/core/src/syscalls.rs::{resolve_user_fault, record_fault_exit}` +
`kernel/core/src/audit.rs` (`AuditEvent::TaskFaultKilled`, id 4034).

## The governing rule (user's mandate, charter-derived)

> Gate for production and debug images so there is **no performance hit in
> prod builds.**

Everything below runs **only on a task that is already dying** — inside
`resolve_user_fault` → `record_fault_exit`, on a task that will never
execute another instruction. A living program pays nothing. So the split
is the same one `plans/FIX-PANICS.md` established, and for the same reason:

- **Zero *CPU* cost on the fault path → EVERY image (prod + debug).**
  Reading facts the kernel already holds, and walking the *already
  maintained* frame-pointer chain, costs a handful of guarded loads once,
  on a halting task. Frame pointers are **already** forced on every
  bare-metal target (`.cargo/config.toml`: `-C force-frame-pointers=yes`
  for `x86_64-unknown-none`, `aarch64-unknown-none`,
  `riscv64gc-unknown-none-elf`), so user programs already maintain the fp
  chain at runtime — walking it adds nothing until crash time.
- **A *size* cost (a symbol table) → DEBUG images only**, gated on
  `cfg!(debug_assertions)`. `ImageProfile::Debug` builds without
  `--release`; `ImageProfile::Installer` builds with `--release`
  (`tools/xtask/src/commands.rs::kernel_build_profile`), so
  `cfg!(debug_assertions)` is the honest gate — exactly as
  `plans/FIX-PANICS.md` uses it. Prod prints load-relative offsets
  resolved offline with `addr2line`; debug resolves names on-target.

The one thing prod builds must **not** do is anything that changes how
running programs are built or scheduled, or that puts work on syscall
dispatch, the capability check, the scheduler, or the allocator (§2.16
hot paths). None of this does.

| Addition | Running-app CPU cost | Prod image | Debug image | Other cost |
|---|---|---|---|---|
| Faulting `name` + `proc_id` + `write` + coarse `fault_offset` bucket | none (fault path only) | ✓ | ✓ | none |
| User-stack fp backtrace (load-relative offsets) | none (fault path only) | ✓ | ✓ | frame pointers **already on** — none new |
| Register snapshot in the crash record | none (read-only, at crash) | ✓ | ✓ | none |
| On-target symbol names (`fn+0x40`) | none (crash path only) | ✗ | ✓ (`cfg!(debug_assertions)`) | image **size** |
| `stderr` breadcrumb from the reaping session | none | ✓ | ✓ | none |

## Address-leak policy (decide explicitly, do not drift)

`AuditEvent::TaskFaultKilled` deliberately omits the raw *user* faulting
address: the shared, hash-chained audit log (§19.4) must not publish ASLR
/ address-space layout. `plans/FIX-PANICS.md` split this the right way —
a *kernel* panic dump (fatal, halting) may carry *kernel* addresses; a
*user*-fault kill still must not carry raw *user* addresses. This plan
keeps that line and never crosses it:

- **On the audit log:** never a raw user address. Only a **coarse,
  offset-relative, non-leaking** descriptor (below).
- **In the capability-gated crash record (System Information API, §16.6):**
  richer detail, but still expressed as **load-relative / region-relative
  offsets**, never absolute user addresses — so even a privileged reader
  gets what it needs to symbolicate offline without the log or the API
  becoming an ASLR oracle.
- **On the crashing program's own `stderr`:** a cause *class* only, never
  an address, never a secret or capability token (§23.1).

## Design — three stages, each a complete, no-stub change (§2.19/§27)

### Stage 1 — Faulting identity + cause on the audit record (every image)

`record_fault_exit(task, fault_va)` already borrows the world; extend the
`AuditEvent::TaskFaultKilled` (id 4034) field set — the ABI is unfrozen,
so evolve it in place and update the asserting tests, never add a v2
(§2.13). New fields:

- `name` — the kernel-attested executable basename. The `ProcName` is
  already on the faulting task's capability record; `CapTable` exposes
  `name()` keyed by task id (`kernel/sec/src/captable.rs`). Never
  caller-supplied bytes.
- `proc_id` — the CSPRNG-minted `ProcId` process-instance identity (also
  on the cap record via `CapTable::proc_id()`), so a crash stays
  correlatable across the log after the scheduler recycles the task id.
- `write` — the `resolve_user_fault(fault_va, write)` flag, already in
  hand and today thrown away. A wild **read** vs a wild **write** sharply
  narrows the bug class.
- `fault_offset` — **not** the raw address. The offset of `fault_va`
  relative to the nearest region the task legitimately owns, or a coarse
  bucket when there is no nearby region:
  - `null_page` — within the first page (offset from VA 0): a
    null-pointer dereference, the single most common `wild` cause.
  - `below_stack_guard` — just below the stack guard page under the
    reserved span (a classic overflow *past* the guard).
  - `region+<off>` — `fault_va` sits a small, bounded offset past the end
    of a specific mapping the task owns ("0x40 past the end of *this*
    region") — the offset is region-relative, so it names *how far past*
    without publishing *where* the region lives.
  - `wild` — genuinely far from every mapping; no meaningful offset.
  The existing `fault_class` (`stack` / `stack_limit` / `file_region` /
  `anon` / `wild`) stays; `fault_offset` refines it.

`name`, `proc_id`, `write`, and the coarse offset bucket are **program
state, not secrets, and carry no address-space layout** (§19.4/§23.1), so
they are safe on the shared log in every image.

`record_fault_exit` currently threads only `fault_va`; it must also
receive `write` from `resolve_user_fault` (a one-arg plumbing change on
the one caller — no ABI creep). Keep the classification allocation-free
and on the fault path only.

### Stage 2 — Register snapshot + user-stack backtrace (every image; symbols debug-only)

This is the payload a developer actually wants, and it lands in a
**capability-gated crash record queryable via the System Information
API** (§16.6) — **never** a `/proc`-style scrape (§16.1 forbids that), and
**never** on the shared audit log (too verbose, too sensitive). The crash
record is a new, versioned, hashed `lib/abi/src/sysinfo.rs` query held to
the §9/§16.6 ABI discipline (frozen on the first release).

**Reuse, do not copy, the panic walker (§2.2/§2.21).** `plans/FIX-PANICS.md`
already landed `tairix_arch_api::backtrace` with a `CpuStateCapture`
(`capture()` + a pure `FrameLayout` / `stack_bounds()`) and a single
audited, bounds-checked, monotonic, depth-capped fp-walk living once in
`kernel/core`, reading memory only through a `StackReader`, plus a fuzz
harness. This stage **extends** that one definition to a *user* variant;
it does not fork it.

The critical difference — and why this needs real care, not a
copy-paste — is that a user fault walks the **user** stack from **kernel**
context, dereferencing **untrusted, possibly-corrupt user pointers**. The
kernel-panic walker reads its own trusted kernel stack. The user variant
therefore MUST:

- Read every frame through the **capability-checked user-access path**
  (`copy_in` / `UserAddressSpace`, `kernel/mem`), never a raw `*(fp)`. A
  corrupt or unmapped fp makes the read return `None` and ends the walk —
  it must **never** fault the kernel (a fault-in-fault is unthinkable; the
  FIX-PANICS "Linus bar"). The `StackReader` seam is now **fallible**
  (`read_word(addr) -> Option<u64>`, done as the Stage 2 prerequisite
  above), so the user variant supplies a `copy_in`-backed reader that
  returns `None` on a faulting copy and the shared walk body is unchanged —
  `walk` already ends cleanly on the first `None`.
- Validate each candidate fp against the task's **own** mapped stack span
  (`aspaces.stack_span(task)`, already tracked): non-null, aligned,
  strictly monotonic (kills cycles), in-bounds. Any failure ends the walk
  cleanly.
- Hard depth cap (reuse the 64-frame cap).
- Emit each frame `pc` as a **program-relative offset from the PIE load
  base**, not an absolute address (same non-leak policy as Stage 1's
  offset). The load base is known to the loader (`rxe` is PIE, §19.2);
  record it once so offsets resolve offline against the unstripped binary
  with `addr2line` — the workflow FIX-PANICS.md already documents.

The **register snapshot** is `CpuStateCapture::capture()` at fault entry
(read-only, no allocation), stored in the crash record as load-relative
`pc`/`sp`/`fp` plus GP registers. Absolute register values are *not* put
on the audit log; they live only in the capability-gated record.

**Capability.** Reading the crash record is a privileged System
Information query. Per the capability-minimalism rule (§5.2), it does
**not** get a brand-new `CAP_*` invented ahead of a holder: it is gated on
an existing `CAP_SYSINFO_*` member (the crash record is process/kernel
introspection — `CAP_SYSINFO_KERNEL` or the introspection gate
`CAP_SYSINFO_INTROSPECT` are the candidates, decided when the query and
its enforcement point land together, not before). A new capability is
justified only if review shows no existing member fits at the right
granularity — recorded in `PLAN.md` if so.

**On-target symbolication** (turning a load-relative offset into
`fn+0x40`) is a **size** cost, not a CPU cost → `cfg!(debug_assertions)`
only, exactly as FIX-PANICS.md gates the kernel symbol table. Prod images
carry no symbol table and print offsets; the offline `addr2line` path is
the complete prod story. **Do the offline cut fully; stage any on-target
table in `PLAN.md`** rather than half-building it (§2.19).

### Stage 3 — `stderr` breadcrumb from the reaping session (every image)

The §2.24 "fail loud, state the reason" obligation: a shell user whose
command segfaults should see *why* on their terminal, not have to trawl
the audit log. The **observer** reports it — the kernel hands the reaping
shell/session the fault *cause class* with the `139` status, and the
session writes a concise line to the crashed program's `stderr` (fd 2,
§20), e.g.:

```
elsh: <name>: killed by fault (wild write near null)
```

The breadcrumb carries the cause **class** only (from Stage 1's
`fault_class` + `write` + coarse offset bucket) — **never** an address, a
register, a secret, or a capability token (§23.1). Where no terminal
consumer exists (a daemon / detached session), the observing component
records the termination through `lib/log` instead, best-effort on both
channels (§2.24). It never carries the Stage 2 detail (that is behind the
capability gate).

## Correctness constraints (the Linus bar — non-negotiable)

1. **No allocation on the fault path.** Everything is stack-buffered, as
   `record_fault_exit` already is. The user-stack walk allocates nothing
   (§4, §2.9).
2. **Every user frame pointer validated before dereference — through
   `copy_in`, fail closed, stop the walk, never fault the kernel.** This
   is the crux and the reason the user variant is *not* a copy of the
   kernel walker: the pointers are untrusted (§5.4).
3. **Bounded depth (64) and strictly monotonic fp** — a
   corrupt-but-plausible chain must terminate; cycles are killed.
4. **No raw user address on the shared audit log, ever** — Stage 1/3 emit
   only coarse non-leaking descriptors; absolute detail lives only in the
   capability-gated Stage 2 record, and even there as load-relative
   offsets.
5. **Blast radius unchanged.** This is pure diagnostics; the containment,
   the `139` status, and `reclaim_task_resources` are untouched. A crash
   inside the diagnostics path must degrade to "no backtrace", never
   widen the kill or take down the kernel.
6. **Prod pays nothing.** No running-program CPU cost; the only
   image-conditional artefact is the debug-only symbol table.

## Deliverables (staged; each complete, no no-ops, no stubs)

- **Stage 1** (`kernel/core/src/syscalls.rs`, `kernel/core/src/audit.rs`):
  thread `write` into `record_fault_exit`; look up `name`/`proc_id` via
  `CapTable`; compute the coarse `fault_offset` bucket; add the
  `name` / `proc_id` / `write` / `fault_offset` fields to
  `AuditEvent::TaskFaultKilled`. Update the `record_fault_exit` rustdoc
  and the audit-event doc page.
- **Stage 2** (`lib/abi/src/sysinfo.rs`, `kernel/arch/api/src/backtrace.rs`,
  `kernel/core`): the versioned capability-gated crash-record query; the
  `copy_in`-backed `StackReader` user variant over the *existing* shared
  fp-walk; the register snapshot; load-relative offsets; the
  `cfg!(debug_assertions)` symbolication gate. New ABI surface — record
  it per §9/§16.6 and add its `PLAN.md` entry.
- **Stage 3** (`userland/session/login`, `userland/shell/shell`, and the
  kernel→observer cause hand-off): the `stderr` breadcrumb / `lib/log`
  fallback carrying the cause class only.

## Tests (§7 — part of each stage)

- **Stage 1:** the four existing `fault_class` arms gain assertions for
  the new fields; add a **null-deref** case (`fault_offset = null_page`,
  `write = false`) and a **wild-write** case (`write = true`), asserting no
  raw address ever appears in the record.
- **Stage 2:** host unit tests for the user-stack walker driving a mock
  `copy_in` reader — normal chain, corrupt/unreadable fp (returns `Err`,
  walk ends, kernel never faults), cycle, unaligned, out-of-bounds vs
  `stack_span`, depth cap; a **fuzz harness** for the user fp-walk fed
  adversarial chains (§19.6 — must always terminate, never deref an
  invalid/unreadable pointer); a QEMU vertical asserting a crashing test
  program yields a crash record with ≥ 2 load-relative frames; the
  capability gate denies an uncapped reader (fails closed, §5.4).
- **Stage 3:** the reaping session writes the cause-class line to the
  crashed child's `stderr`; the daemon/detached path logs via `lib/log`;
  neither ever emits an address or secret.

## Docs (§2.8, §13)

- Rustdoc on `record_fault_exit`, the crash-record query type, and the
  user-stack walker variant.
- A `docs/src/architecture/fault-diagnostics.md` page (sibling to
  `panic-diagnostics.md`): what a user-fault kill emits, the every-image
  vs debug-only split, the address-leak policy, the capability gate, and
  the offline `addr2line` workflow for load-relative offsets.
- `README.md` support-matrix row if warranted (§13).

## Definition of done (per stage)

The §2.15 / §7 whole-project gate is green (`cargo fmt --all`, one
`cargo xtask ci`, `cargo xtask fuzz --secs 5`, the
`tools/ci/soak.sh both --secs 20` smoke), output quoted in the completion
report, and the §23 self-review verdict stated. Prod images gain the
identity/cause/backtrace-offset detail at **zero** running-program cost;
only debug images additionally ship the on-target symbol table.
