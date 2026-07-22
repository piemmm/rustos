# FIX-WILD — Debuggable user-fault kills (identity, cause, backtrace)

Status: **planned** — the user-fault containment path is already correct
(it isolates the crash to one task, classifies the address without leaking
layout, audits it with a stable id, records a `139` `wait` status, and
reclaims resources). This plan makes that *correct* path **debuggable**,
without adding a single cycle to any running program's hot path.

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
  corrupt fp returns `Err` and ends the walk — it must **never** fault the
  kernel (a fault-in-fault is unthinkable; the FIX-PANICS "Linus bar").
  The `StackReader` seam already abstracts "read a word of stack memory",
  so the user variant supplies a `copy_in`-backed reader and the shared
  walk body is unchanged.
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
