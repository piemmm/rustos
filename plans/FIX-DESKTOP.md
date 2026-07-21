# FIX-DESKTOP.md — Non-blocking desktop: asynchronous process launch

Binding under `AGENTS.md`. This plan removes the desktop freeze that
occurs while an application is loaded and started, fixes it as a
first-class property of the process-launch path (not a desktop-local
band-aid), and stages the identical fix for every other interactive
loop that inherits the same defect.

The rule this plan enforces is already in the charter: an interactive
loop must never stall on work an unrelated task could do
(`§2.16`, `§2.23`, `§26.2`), and I/O for one activity must never freeze
another (`§26.1`). The launch path violates it today.

---

## 1. The defect

Launching an app from the desktop freezes the whole desktop until the
app has finished loading and starting.

Root cause — the launch is done **synchronously, on the caller's own
task, inside one `SyscallNumber::SPAWN` invocation**:

1. `SysHandlers::spawn` (`kernel/core/src/syscalls.rs`) resolves the
   path, then calls `load_store_bundle` → `tairix_appload::AppLoader`,
   which **reads the whole `<Name>.app/Run` rxe from the mounted VFS**
   and **verifies its signature + content/interface hashes**. Only a
   `LaunchCache` hit (a re-launch of an immutable, already-verified
   bundle in the same boot) skips the *verification*; the VFS read
   authorisation and the address-space build are never skipped.
2. `ProcessSpawn::spawn_with` (the per-arch producer) then **eagerly
   builds the child's hardware-isolated address space** from the rxe
   (`build_process_image` in `kernel/mem`), copying every loadable
   segment into freshly allocated frames, before the child is admitted.
3. Both (1) and (2) run on the **calling task**. Per `§17.1` (and
   `plans/FIX-SYSCALL.md` P-5b) the kernel is non-preemptible *per
   task*: a syscall body runs to completion for that task before it
   returns to user mode. Device IRQs and the preemption tick are
   deliverable (P-5b), so the *machine* is not wedged — but the
   **desktop's own compositor task** does not return to its
   present/input loop until the launch finishes. To the user the
   desktop is frozen: no repaint, no cursor motion, no input, for the
   whole duration of a disk read + signature verification + image
   build.

The freeze is worst on the first launch of a bundle in a boot (full
read + verify + build) and still present on a cached re-launch (VFS
read re-authorisation + full image build).

### 1.1 Where it bites (audit)

Every caller that issues a blocking launch **from an interactive
event loop** inherits the freeze:

| Site | File | What blocks |
|---|---|---|
| Desktop launcher | `userland/gui/session/src/run.rs` (`route_outcome`, the `FILES_/TERMINAL_/VIEWER_LAUNCHER` arms — `tairix_rt::spawn(...)`) | Compositor loop frozen for the whole launch. **The reported bug.** |
| Desktop file picker | `userland/gui/session/src/run.rs` (the `SessionPicker`'s `VfsDirectorySource` calling `tairix_rt::read_dir_all` inside `picker.handle_click` / `handle_key`, on the `SEAT_TOKEN` path) | Compositor loop frozen while a directory is read from disk on every open/navigate. Same class of defect (synchronous I/O on the compositor thread), smaller blast radius. |
| Shell foreground launch | `userland/shell/elsh/src/run.rs` (`spawn_attached`) | The shell cannot service its own input (job-control signals, `stdinfo`) during the load. Secondary. |
| Terminal startup shell | `userland/apps/terminal/src/run.rs` (`spawn_attached`) | One-time, at terminal open. Minor. |
| Login → session/shell | `userland/session/login/src/run.rs` (`spawn_as` / `spawn_with`) | One-time, at login. Minor. |

`init` (`userland/system/init`) launches at boot, not from an
interactive loop; it is out of scope except that it benefits from the
same async-launch semantics for free.

Non-issues (kept synchronous deliberately): the desktop's window
endpoint `call_recv` (only dequeues an already-peeked call — bounded),
and the display `present` IPC (bounded, and the protocol serialises the
frame region). These are not part of this fix.

### 1.2 The second defect — eager, unshared image build

Deferring the load off the caller (§2) fixes *responsiveness*: the
desktop no longer freezes. It does **not** fix the *cost* of the load,
because `build_process_image` (`kernel/mem/src/spawn.rs`) is **eager**
— it allocates a fresh frame for every loadable segment page and copies
the segment bytes into it *before* the child enters user mode — and
**unshared**: two instances of the same bundle, and even one large
bundle whose pages are never touched, each pay the full copy into
private frames.

Two consequences follow, and both are ground TAIRiX must not cede to
Linux (`§26.6`, `§26.7`):

- **Up-front latency and memory proportional to the *whole* binary, not
  the working set.** A large `Run` rxe stalls the child's first slice
  building pages it may never execute, and pins a private frame per page
  regardless of use. Linux does not do this: `execve` maps the image and
  faults pages in **on demand** from the page cache, so cost tracks the
  working set.
- **No sharing between instances.** Every launched copy of the same
  bundle gets its own private frames for identical, immutable read-only
  text/rodata. Linux shares those pages across every process running the
  same binary via the page cache; N instances cost ≈ one copy of the
  read-only image plus each instance's private writable pages.

Left only-deferred, this fix would be *on par with* Linux for
responsiveness yet *behind* it on the "huge working set / many instances
on a small machine" axis (`§26.6`, `§26.7`). This plan therefore also
replaces the eager build with a **demand-paged, copy-on-write,
verified-once shared image** (§2.6). That is **strictly ahead** of
Linux: it keeps Linux's demand-paging and read-only sharing *and* adds
per-installation signature/hash verification the page cache does not
perform — without ever mapping an unverified byte.

---

## 2. Design — asynchronous process launch (first-class)

The fix is a property of the launch path, so every caller benefits and
no interactive loop needs a special case.

**`SPAWN` admits the child immediately and returns its PID; the child
loads its own image on its first scheduled slice, on the child's own
task — never the parent's.** The parent (desktop, shell, …) returns from
`spawn` at once and keeps running; the slow work (VFS read,
verification, image build) happens as the child's own kernel-side
bring-up, preemptible against the parent and free to run on another CPU.

This mirrors `fork`+`exec`: the child does the `exec` load, not the
parent. It reuses seams that already exist:

- The admit path (`KernelSpawnCtx` / `admit_process`) already records
  the **parent/child wait link**, the kernel-attested **credential**,
  the minted **`proc_id`**, the attested **name**, and the standard
  streams — all resolved **synchronously** at spawn time and **kept**.
- A user task carries a **`pre_resume` / `work`** body that runs in the
  task's *own* context before it enters user mode
  (`kernel/core/src/kthread.rs`). The image load moves **into** that
  body.

### 2.1 What stays synchronous on the caller (fail-fast, fail-closed)

These are cheap and must answer the caller directly, so they stay in the
`SPAWN` handler on the calling task, **before** any child is admitted —
preserving `§5.4` (capability check before state, validate every input,
fail closed):

- `CAP_PROC_SPAWN` (already checked by the dispatcher).
- `path` copy-in + length bound + `SPAWN_SELF` substitution.
- Path **syntax** resolution: is this a boot-floor registry name or a
  well-formed `<Name>.app/Run` bundle path? A syntactically invalid
  path still returns `NotFound` synchronously.
- The attach/startup-strings block validation and the standard-stream
  wiring (owner-checked parent descriptors) — unchanged, synchronous.
- Credential resolution (`resolve_spawn_credential`, incl. the
  `CAP_SPAWN_AS_USER` gate) — unchanged, synchronous.
- `proc_id` mint, `ProcName`, resolved `spawn_path` — unchanged.

### 2.2 What moves into the child's own bring-up (deferred, off the caller)

Performed by the admitted child's `work`/`pre_resume` body, on the
child's task, before it enters user mode:

- `wait_app_store` (park until `/System` is mounted) — already a park;
  parking the *child* costs the parent nothing.
- `load_store_bundle`: the VFS read of the entry point and the
  `AppLoader` signature/hash verification (with the existing
  `LaunchCache` still hoisting verification off re-launches). The VFS
  read is re-authorised under the child's own kernel-attested
  credential (`§5.4`), which is *more* correct than authorising under
  the caller.
  - **Under which authority (resolved).** The child's credential
    (`SpawnCredential`, resolved synchronously at admit — §2.1) carries
    the child's `uid` and its account **capability ceiling** (the stored
    user ceiling for an inherit spawn; the target user's ceiling for a
    `CAP_SPAWN_AS_USER` switch). The bundle read is authorised as
    `(uid = credential.uid, effective = credential.ceiling)`. The
    ceiling is the right authority precisely because the app's *own*
    effective set is `ceiling ∩ manifest`, which is not yet known at
    read time (the manifest is what is being loaded) — and "may this
    user read this bundle to launch it" is bounded by the user's account
    grant (the ceiling), not by the app's post-load effective set. A
    system-principal credential with no ceiling authorises the read
    under its system identity exactly as its synchronous predecessor
    did. This is never *wider* than the account allows and fails closed
    on a refused read (the child exits `LOAD_NOT_FOUND`, §2.3).
  - `load_store_bundle` therefore takes an explicit
    `(uid, effective_caps)` pair rather than a borrowed `CallerContext`,
    so the same code path serves both the (now historical) caller-side
    read and the child-side read under the child's own credential — one
    definition, no fork (`§2.2`).
- The image is **prepared** into the child's fresh address space —
  segment regions reserved and, per §2.6, either faulted in on demand
  from the verified shared image or (until §2.6 lands) eagerly built.
  Whichever form, it happens on the child's task, not the parent's.

The child does not enter user mode until its image is fully **verified**
(the whole content hash checked — §2.6) and its address space is
**mapped**. **No unverified byte is ever mapped and no unbuilt page is
ever executed** — the security invariant is unchanged; only the *task on
which the load runs*, and (per §2.6) *when each page is materialised*,
change.

### 2.3 Failure semantics (fail loud, `§24`)

Because the heavy, I/O-dependent failures (missing bundle bytes,
tampered signature, malformed rxe, OOM during build) are now discovered
by the child, they surface via the child's **exit**, not the `spawn`
return value:

- The child that fails to load exits with a **reserved, distinct exit
  status per cause**, and the kernel **audits** the load refusal through
  `lib/log` with a stable event id (the audit that `load_store_bundle`
  already emits, now attributed to the child).
  - **Concrete statuses (resolved).** Exit codes are `i32`
    (`WaitStatus::Exited(i32)`, `lib/abi/src/process.rs`). The reserved
    load-failure statuses are a closed set of named `i32` constants in
    `lib/abi` (`process.rs`), sitting in a high, reserved band that a
    normal program's own `exit(code)` does not use — `LOAD_NOT_FOUND`
    (missing bundle / refused read), `LOAD_UNVERIFIED` (signature /
    content-hash / interface-hash mismatch), `LOAD_MALFORMED`
    (un-parseable rxe / CFI-tag mismatch / layout unfit), and `LOAD_OOM`
    (frame / page-table exhaustion during build). A `spawn`-caller
    `Errno` maps deterministically onto exactly one of these through a
    single shared `lib/abi` function (`load_failure_status(errno)`), and
    a matching `lib/abi` reverse map (`load_failure_reason(code) ->
    Option<&'static str>`) turns a reaped code back into the terse
    human-readable reason a parent prints on `stderr`. Both live in
    `lib/abi` so producer (child load path) and consumer (parent reap)
    can never diverge (`§2.2`), and both are exercised by `lib/abi`
    unit tests (round-trip: every cause maps to a status and back to a
    reason; a non-load exit code maps to `None`).
- The parent observes the failed child through the **normal child-exit
  path it already has** (the desktop's `CHILD_TOKEN` reap; a shell's
  `wait`). The desktop already tears an exited client's windows down and
  can report "launch failed" on `stderr` (`§24.1`), turning a silent
  freeze into a loud, non-fatal diagnosis.

This is an intentional, pre-release ABI semantics change (`§2.13`): the
`spawn` return value means **"child admitted"**, not "child loaded".
Syntactic/authority failures remain synchronous `-errno`; I/O and
verification failures are reported via child exit + audit. The change is
made **in place**; no `spawn2` alias, no compatibility shim.

### 2.4 Alternatives considered and rejected

- **A dedicated in-kernel launch kthread pool** that loads on behalf of
  the caller. Rejected: it duplicates the task the child already is,
  needs its own back-pressure/queue, and must hand the built space to a
  *different* task at the end — more moving parts than having the child
  load itself, for no gain (`§2.3`).
- **A user-space launcher service** the desktop messages to spawn apps.
  Rejected: it makes the launcher the parent, breaking the desktop's
  parent/child window-teardown (`CHILD_TOKEN`) and `wait` reaping, and
  re-introduces a blocking hop somewhere. The kernel-side async spawn
  keeps the desktop the parent and changes only *timing*.
- **Spawning on a second desktop thread.** Rejected: userland has no
  thread primitive, and it would still block *a* desktop task on I/O;
  the defect belongs to the launch path, and that is where it is fixed.

### 2.5 The picker listing (same defect, same principle)

The file picker's directory read (`read_dir_all`) runs on the
compositor thread. The first-class fix is symmetric: a directory listing
that backs an interactive UI must not block the UI loop.

Two acceptable resolutions, decided in DESK-4:

- Make `read_dir_all` / the browse `DirectorySource` **incremental and
  bounded** so a listing is drained across wait-set wakes (the browser
  engine already pages large listings), never in one blocking call; or
- Serve the picker's listing over the same async pattern (a listing the
  session requests and drains without parking the compositor).

The chosen form must keep the picker's existing capability discipline
(the session lists under its own authority; the app lists nothing) and
its fail-closed behaviour (a refused listing shows nothing, never a
guess).

### 2.6 Beyond Linux — demand-paged, copy-on-write, verified shared images

This is the second half of the fix (§1.2). It replaces the eager,
per-child `build_process_image` copy with a demand-paged image backed by
a **verified shared image cache**, so program loading is *strictly ahead*
of Linux: Linux's demand-paging + read-only sharing, plus verification
Linux does not do, with no unverified byte ever reachable.

It builds on mechanisms the kernel already has, so it is not a new
subsystem invented speculatively (`§2.3`): the demand-page fault path
(`kernel/mem/src/filemap.rs` — reserve address space at map time, back
one page per fault, zero-on-free, sparse teardown), the fresh-frame
discipline in `anon.rs`, the reclaim/pressure machinery
(`kernel/mem/src/pressure.rs`, `reclaim.rs`), and the per-boot
`LaunchCache` that already retains a verified `LoadedApp`.

#### 2.6.1 The verified shared image cache (`kernel/mem`)

A single kernel-owned, per-boot cache maps a **verified content hash**
(the `content_hash` `AppLoader` already computes and checks against the
signed manifest — `lib/appload/src/loader.rs`) to a set of **immutable,
physical, read-only backing frames** holding the bundle's read-only
segments (text, rodata) and the byte-exact initial content of its
writable segments. Keying on the *verified* hash is what makes sharing
safe: a frame set exists in the cache **only after** the whole content
hash matched, so anything faulted from it is provably the signed image
(`§5.4`, `§19` — this generalises `filemap`'s "no user-visible frame
before it holds exactly the intended bytes" to "no shared frame before
the whole image is verified").

- **First launch of a bundle:** the child reads and verifies the rxe
  (§2.2), then *populates* the cache: it allocates the read-only backing
  frames, fills them from the verified bytes through the kernel physical
  map (as `spawn.rs` does today — a kernel-side write, never a user-
  writable page, preserving W^X), and marks them immutable. This
  subsumes today's `LaunchCache`, which becomes the metadata handle onto
  a cached frame set rather than a separate retained `LoadedApp`
  (`§2.2`, `§2.14` — one cache, not two).
- **Subsequent launches / additional instances:** the child finds the
  verified frame set by hash and maps it **without re-reading disk and
  without re-copying** — the page-cache-equivalent sharing Linux gets,
  and *faster than a re-verify* because the hash match already happened.
- **Refcounted lifetime, reclaimable under pressure (`§26.3`,
  `§24.1`).** Each frame set is reference-counted by its live mappers.
  A set with no live mappers is a clean, reclaimable cache entry: under
  memory pressure the existing reclaim path (`reclaim.rs`) drops it
  (frames scrubbed on free, `§4`), and the next launch re-populates it.
  The cache is a bounded, growable capacity sized from discovered RAM —
  never a fixed per-bundle slab or a whole-`/System` resident copy
  (`§24.1`, `§26.6`, `§26.7`).

#### 2.6.2 Demand paging (fault-in, not eager copy)

The child's bring-up (§2.2) **maps** the segment regions but populates no
page eagerly. A segment region is registered against the child's live
space (the `filemap` region-table pattern), and the first access to a
page faults:

- **Read-only / execute pages (text, rodata):** the fault resolves to the
  **shared** cached frame (§2.6.1), mapped read-only (RX for text, R for
  rodata) into the faulting space. No copy, no allocation — the frame is
  shared with every other instance. This is the sharing win *and* the
  demand-paging win at once: a page never executed is never mapped, and a
  page executed by ten instances is one physical frame.
- **Writable pages (data, BSS):** mapped **copy-on-write**. The initial
  mapping points at the cached initial-content frame (or, for BSS,
  the shared zero frame) read-only; the first *write* faults, allocates a
  private zeroed frame, copies the one page in (break-before-make on the
  arches that require it, via the Arch HAL MMU primitive — `§17.2`), and
  remaps it writable. A writable page never written stays shared; only
  touched-and-written pages cost a private frame. This is `fork`/`execve`
  COW for the *initial* image, done right and once.
- **Guard pages and W^X unchanged.** The stack keeps its guard page
  (`§4`); no fault ever produces a writable-and-executable page (the COW
  break produces R/W data, never RX — the W^X invariant `loader.rs`
  already enforces holds per-page here too).

The one page the child *must* have resident before it enters user mode is
whatever the entry sequence and the startup block touch first; everything
else is faulted lazily. The startup-vector block (`spawn.rs`) stays a
freshly built private page (it is per-launch data — arguments,
environment, canary seed — never shared).

#### 2.6.3 Why this is strictly better than Linux, and still fail-closed

- **Verification the page cache lacks.** Linux's page cache shares
  whatever is on disk; a shared frame here exists only behind a matched
  signed content hash, so demand-faulting can never surface a byte the
  loader did not verify. Sharing and security compose instead of
  trading off.
- **Working-set cost, not whole-binary cost (`§26.6`).** Latency to first
  instruction is one (or few) faults, not a whole-image copy; resident
  memory is touched pages, not the binary. A 100 MiB rxe that runs a
  200 KiB hot path costs ≈ 200 KiB.
- **N instances ≈ one read-only copy (`§26.7`).** Read-only text/rodata
  is one physical frame set for all instances; per-instance cost is only
  written data pages + stack + startup block. Many launches of the same
  app on a small machine stay bounded.
- **Deterministic OOM, never a panic (`§4`, `§2.9`).** A fault that
  cannot allocate (COW break, first-touch data) fails the *faulting
  access* closed — the child takes a fatal fault and exits with the
  reserved `LOAD_OOM` status + audit (§2.3), exactly as an eager build
  OOM would, never a kernel panic and never a partially mapped run.
- **No busy-poll (`§2.23`).** A fault that must read disk to populate a
  not-yet-cached read-only frame parks the faulting task on the VFS I/O
  and is woken on completion; it never spins.

#### 2.6.4 Scope and honesty

Demand-paging the *rxe segments* from the verified cache is the whole of
this section and is fully specified and staged (DESK-5/DESK-6) — there is
no "future work" left implicit. Two things are deliberately **out of
scope and named so**, not silently omitted:

- **Paging shared read-only frames back to disk under extreme pressure.**
  Read-only image frames are *reclaimable* (drop + re-fault from the
  verified bundle, §2.6.1) rather than swap-backed, because the bundle on
  disk is their durable, re-verifiable source — reclaiming and re-reading
  is both cheaper and safer than writing verified code to encrypted swap.
  This is a complete design decision, not a deferral.
- **Cross-*boot* image caching.** The cache is per-boot; the disk bundle
  is the cross-boot source of truth. A persistent verified cache would be
  a distinct feature with its own on-disk-trust story and is explicitly
  not part of the launch fix (stating this satisfies `§15.7`; it is not a
  hidden TODO).

#### 2.6.5 Task-model mechanism — deferred user-space installation

Deferring the load onto the child (§2.2) requires a task-model operation
the kernel did not previously have: **an already-running kernel task
installs its user address space after admission and then enters user
mode.** Today `admit_process` (`kernel/core/src/syscalls.rs`) wires the
child's frozen address space, `stack_span`, live-space slot and
page-table-root `pre_resume` hook into the kthread control block *at
admit time*, so there was no way for a task to become a user process
later. The mechanism this plan adds (arch-neutral, in
`kernel/core/src/kthread.rs`):

- The child is admitted as a **loading kthread** — a normal kernel
  kthread (`pre_resume = None`, no live space, no user address space
  registered), admitted parked and unparked exactly as any child, so no
  CPU can dispatch it before its per-task admit state exists.
- **Admit-time per-task state (synchronous, on the caller).** Under the
  child's freshly minted `sec_id` the caller installs everything that is
  known without the manifest: a **loading** capability record carrying
  the child's kernel-attested identity (`proc_id`, `name`, `spawn_path`,
  parent link, credential = uid + gids + ceiling, sandbox brand) but an
  **empty** capability set — a loading kernel kthread wields no user
  authority — plus streams, wired std entries, inherited limits, and cwd,
  the parent/child wait link, and any device-resource grants. The
  **address space is *not* registered here** (the child has none yet).
  This is why the plan's "resolved at admit" invariant (§3) holds for
  *identity, credential, streams, parentage, limits* — the fields that do
  not depend on the manifest — while the manifest-derived capability set
  is installed by the body below, before the child can run a single user
  instruction.
- Its **work body** (assembled in `kernel/core`, capturing the `'static`
  load services + an arch image-builder seam) runs on the child's own
  task and performs, in order: `wait_app_store` → `load_store_bundle`
  (VFS read + signature/hash verify, under the **child's own**
  kernel-attested credential — §2.2) → derive the child's **effective
  capability set** (`credential.ceiling ∩ manifest request`) and
  **replace** the loading record's empty set with it under `sec_id` → the
  arch image build → register the frozen address space + `stack_span`
  into the address-space registry under the child's own id →
  `Yielder::become_user(pre_resume, live)` → `enter_user`. The capability
  set and the user address space are both installed strictly *before*
  `become_user`, so the child holds exactly `ceiling ∩ manifest` and its
  own mapped space the instant it becomes dispatchable as a user task —
  never a window where it runs user code under the wrong authority
  (`§5.4`).
- `Yielder::become_user` deposits a `UserUpgrade { pre_resume, live }`
  into the control block's new `pending_upgrade` slot and **yields**
  (re-enqueue, not park). On the next dispatch, `dispatch_step` installs
  the deposited hook + live space *dispatcher-side* (the same side that
  already mutates these fields), so the task resumes as a fully-formed
  user kthread — activates its root, publishes the syscall resume handle
  and live-space pointer — then the body resumes past `become_user` and
  calls `enter_user`. The window between resume and `enter_user` is the
  identical kernel window a normal user task's `work = { enter(); }`
  body already has, so no new invariant is introduced.
- **Failure** at any step before `become_user`: the body audits the load
  refusal through `lib/log` (attributed to the child) and calls
  `exit(status)` with the reserved `LOAD_*` status for the cause (§2.3).
  No user space is ever registered, no unverified byte is ever mapped,
  and the parent observes the failure through its normal child-exit reap.
- The arch build is the **only** arch-specific piece, behind an
  `ArchImageBuilder` seam (`kernel/core`): given the verified `rxe`
  bytes + caps + args + env it produces the `BuiltImage { frozen,
  physmap, stack_span, live, pre_resume, enter }`. `ProcessSpawn`
  therefore changes from "build+admit synchronously given rxe" to
  "hand the core an image builder"; the core owns the admit + loading-body
  orchestration so the deferral logic has one arch-neutral definition
  (§2.20, §2.21). The kernel stack the loading body runs on is allocated
  by the arch seam at admit (arena or `BoxStack`); the guard page is
  re-expressed in the child's own root during the arch build, exactly as
  the eager producer does today.

---

## 3. Invariants (must hold after the fix)

- **Security unchanged.** `CAP_PROC_SPAWN` and credential resolution
  stay synchronous, before any child state (`§5.4`). Signature/hash
  verification stays mandatory and happens **before** the child enters
  user mode. The child's VFS read is authorised under the child's own
  attested credential. Fail closed on every error path (`§5.4`, `§2.9`).
  No ambient authority (`§4`).
- **No busy-poll anywhere.** The child *parks* on VFS I/O and on the app
  store latch; the parent *parks* on its wait-set. No new spin
  (`§2.23`).
- **Parent/child semantics preserved.** `wait`/reap, the desktop's
  `CHILD_TOKEN` teardown, streams inheritance, sandbox branding, and
  resource-limit inheritance all keep working (they are resolved at
  admit, which is unchanged).
- **Multi-arch, one definition.** The load-deferral logic is
  arch-neutral (`kernel/core` + `kernel/mem`); only the genuinely
  target-divergent producer glue lives per arch (`§2.20`, `§2.21`,
  `§17.2`). No `cfg(target_arch)` leaks outside `kernel/arch/<target>/`.
- **Deterministic OOM.** A build (§2.2) or a demand fault / COW break
  (§2.6) that cannot allocate frames fails the child closed (a reserved
  exit + audit / a fatal fault → exit), never a panic (`§4`, `§2.9`).
- **`§26` load.** Many launches in flight are many admitted children
  each doing bounded, parked I/O; per-child cost is only its *touched*
  pages (§2.6), reclaimable under pressure, never a per-launch fixed slab
  on the parent.
- **No unverified byte is ever mapped (§2.6).** A shared image frame
  exists only behind a matched signed content hash; demand-faulting and
  COW never surface a byte the loader did not verify. Sharing composes
  with verification, it does not weaken it (`§5.4`, `§19`).
- **Read-only image pages are shared; writable pages are copy-on-write
  (§2.6).** Identical text/rodata is one physical frame set across all
  instances of a bundle; a writable page costs a private frame only once
  it is actually written. W^X and the stack guard page are unchanged; a
  COW break never yields an executable page.
- **The image cache is a bounded, growable, reclaimable capacity
  (`§24.1`, `§26.3`).** It is sized from discovered RAM and its unmapped
  entries are reclaimed under pressure (frames scrubbed on free), never a
  fixed per-bundle slab nor a whole-`/System` resident copy; a small
  machine mounting large stores stays within its RAM (`§26.6`, `§26.7`).

---

## 4. Stages

Each stage is independently reviewable and must leave the whole-project
`§7` gate green before it is reported done.

### DESK-1 — Defer the image load into the child's bring-up (core)
- **Deliverables:** `SPAWN` splits into (a) synchronous admit of a child
  in a *loading* bring-up state carrying the resolved path/credential/
  streams/proc_id, returning the PID; (b) the child's `work`/`pre_resume`
  body performing `wait_app_store` → `load_store_bundle` →
  `build_process_image` → enter-user, or exiting with a reserved
  load-failure status + audit on any refusal. `kernel/core/src/spawn.rs`
  + `kernel/core/src/syscalls.rs` + `kernel/mem` build seam; the per-arch
  `ProcessSpawn` producers admit-then-load rather than build-then-admit.
  This stage keeps the existing eager `build_process_image` as the image
  step (moved onto the child); DESK-5/DESK-6 then **replace** it with the
  demand-paged shared image (§2.6) — the eager build is superseded, not
  kept alongside (`§2.14`).
- **ABI:** document the new `spawn`-return meaning ("admitted") and the
  reserved load-failure exit statuses in `lib/abi` + `docs/src/abi/` and
  `docs/src/architecture/syscalls.md`. Regenerate the C header
  (`cargo xtask c-header --write`); `abi-check` clean.
- **Tests (host):** admit returns a PID without touching the bundle
  bytes; a missing/tampered/malformed bundle admits then exits with the
  right reserved status and emits the audit event; a valid bundle loads
  and enters user; OOM during build exits closed; credential is resolved
  synchronously and the child reads under it.
- **Tests (QEMU, per arch):** an app spawned from a session loads and
  runs; a bad bundle path admits then exits observably; the existing
  spawn/session verticals still pass.

### DESK-2 — Desktop launcher no longer freezes (proves the fix)
- **Deliverables:** `userland/gui/session/src/run.rs` launcher arms are
  unchanged in code (they already call `spawn` and reap via
  `CHILD_TOKEN`) but now return immediately; a child that exits with a
  load-failure status is reported on `stderr` (`§24.1`) instead of
  vanishing silently. Add the reserved-status → message mapping.
- **Tests:** a QEMU desktop vertical that launches an app and asserts the
  compositor keeps presenting/handling input during the load (a present
  count / input echo advances while a child is mid-load); a refused
  launch surfaces the diagnosis and the desktop survives.

### DESK-3 — Same fix, other interactive loops
- **Deliverables:** confirm `elsh` foreground launch and the terminal
  startup benefit with no code change (they already reap), and add the
  load-failure diagnosis where a caller previously relied on a
  synchronous `-errno` for an I/O/verification failure. Update
  `plans/SHELL.md` / `plans/APPS.md` cross-refs if the launch-failure
  reporting wording changes.
- **Tests:** shell reports a failed foreground launch with its reason;
  job control stays responsive during a load.

### DESK-4 — Picker listing off the compositor thread
- **Deliverables:** make the picker's directory listing non-blocking per
  §2.5 (incremental/bounded drain, or async listing), in `lib/browse` /
  the session picker, preserving the capability + fail-closed discipline.
- **Tests:** navigating a large directory in the picker does not block
  the compositor (present/input advances mid-listing); a refused listing
  shows nothing.

### DESK-5 — Verified shared image cache (`kernel/mem`)
- **Deliverables:** the per-boot, content-hash-keyed verified image
  cache of §2.6.1: a `kernel/mem` structure mapping a verified
  `content_hash` to a refcounted set of immutable read-only backing
  frames (text/rodata + the byte-exact initial content of writable
  segments), populated on first verified launch and mapped by hash on
  subsequent launches. Fold today's `LaunchCache` into it (one cache, not
  two — `§2.14`): `LaunchCache` becomes the metadata handle onto a cached
  frame set. Refcount by live mappers; wire empty entries into the
  existing reclaim/pressure path (`reclaim.rs`, `pressure.rs`) with
  frames scrubbed on free (`§4`); size the cache from discovered RAM
  (`§24.1`).
- **Tests (host):** first populate fills frames from verified bytes;
  second lookup by hash returns the same frame set with an incremented
  refcount and touches no disk; a content-hash mismatch never populates
  (nothing cached, nothing served); dropping the last mapper makes the
  entry reclaimable and reclaim scrubs the frames; a tampered byte
  changes the hash so it can never collide with a verified entry.
- **Tests (QEMU, per arch):** two instances of one bundle share their
  read-only frames (resident frame count for the read-only image does
  not double); under induced pressure an unmapped entry is reclaimed and
  the next launch re-populates it.

### DESK-6 — Demand paging + copy-on-write (replaces the eager build)
- **Deliverables:** the fault-in image of §2.6.2, replacing the eager
  `build_process_image` copy from DESK-1. The child's bring-up **maps**
  segment regions (the `filemap` region-table pattern over
  `LiveSpace`) and populates no page eagerly; the page-fault path
  resolves a read-only/execute fault to the **shared** cached frame
  (DESK-5) mapped R/RX, and a writable segment page **copy-on-write**
  (initial mapping read-only at the cached/zero frame; first write
  allocates a private zeroed frame, copies the page, break-before-make
  via the Arch HAL MMU primitive, remaps writable). The startup-vector
  block stays a private per-launch page (`spawn.rs`). Delete the eager
  segment-copy path once nothing calls it (`§2.14`). Arch-neutral fault
  logic in `kernel/mem` + `kernel/core`; only the genuinely divergent
  fault-decode/MMU glue per arch (`§2.20`, `§2.21`, `§17.2`).
- **Tests (host):** a mapped-but-untouched page consumes no frame; a
  read fault to text maps the shared frame (no copy, no alloc); a write
  to a data page breaks COW into a private frame preserving the initial
  bytes; a never-written data page stays shared; a fault that cannot
  allocate fails the access closed (no partial map); no fault ever
  produces a W+X mapping; the stack guard page still faults fatally.
- **Tests (QEMU, per arch):** a large bundle reaches its entry point
  after only a few faults (working-set resident, not whole-binary); many
  instances of one bundle keep read-only frames shared; OOM mid-fault
  exits the child with `LOAD_OOM` + audit, never a panic; the
  spawn/session verticals still pass.

### DESK-7 — Docs, README, gate
- **Deliverables:** `docs/src/architecture/` (process launch is
  asynchronous **and** demand-paged / CoW-shared and verified —
  strictly ahead of Linux, §2.6.3), `docs/src/abi/` (the admitted-vs-
  loaded `spawn` semantics and reserved statuses), the `README.md`
  matrix if a per-arch mark changes, and this plan collapsed to its
  done-state. Full `§7` gate green over the whole workspace, output
  quoted in the completion report.

---

## 5. Status

- **Audit — done** (§1). Both defects (freeze §1.1, eager/unshared build
  §1.2) and every affected interactive loop identified; the design is
  fully specified through demand-paged, CoW-shared, verified images
  (§2.6) — no "future work" left implicit (§2.6.4).
- **DESK-1 — in progress (multi-session atomic change).** The
  deferred-load design is fixed (§2.6.5: loading kthread → child-side
  load+build → `become_user` upgrade → enter-user, or exit with a
  reserved `LOAD_*` status + audit). The task-model primitive and the ABI
  contract it exits through are landed; the remaining producer/handler
  rewire is a **single atomic change** (the `ProcessSpawn`/admit contract
  ripples to all arch producers + the driver-spawn loader at once), so the
  tree does not build until it completes and it lands across sessions.
  The full remaining mechanism has since been **validated end-to-end
  against the live code** (the `spawn` handler, `KernelSpawnCtx`,
  `admit_process`, `reclaim_task_resources`, `load_store_bundle`,
  `wait_app_store`, `dispatch_step`, and all three arch producers): the
  three "**Validated:**" refinements in the "Remaining" items below —
  a *non-generic* `SpawnServices` fronted by a `SpawnRuntime` A-eraser, a
  complete failed-loading-child teardown factored out of
  `reclaim_task_resources`, and a fail-closed guard-unmap in a mechanical
  producer split — are the resolved contract the atomic change is built to.
  The build-breaking part is genuinely all-or-nothing: **deleting**
  `ProcessSpawn`/`admit_process` and rewiring the producers breaks every
  arch producer + the driver/init path + ~15 QEMU crates at once, so that
  deletion + rewrite lands as one dedicated change, not partially. The
  arch-neutral *seam types* (item 1), by contrast, are exported trait/struct
  definitions the imminent producer rewrite will implement — landing them
  ahead of the wiring keeps the tree green and is a reserved contract, not
  forbidden dead code, exactly like the already-landed task-model primitive
  and `LOAD_*` ABI. The `§2.3`/`§2.4` concern binds the *deletion + rewrite*
  step (which must be complete, never partial), not the reserved-contract
  definitions.
  - **Landed — task-model primitive.** The arch-neutral primitive in
    `kernel/core/src/kthread.rs`: `UserUpgrade`, the
    `ThreadControl::pending_upgrade` slot, `Yielder::become_user`, and the
    dispatcher-side install in `dispatch_step`, with host tests.
  - **Landed — the arch-neutral image-build seam (Remaining item 1).**
    `BuiltImage`, `ImageBuildCtx`, and `ArchImageBuilder` in
    `kernel/core/src/spawn.rs`, exported from `lib.rs`. The deferred-load
    replacement for `ProcessSpawn`/`SpawnCtx::admit_process`: the arch seam
    builds an isolated image off the loading child's own stack and returns
    it as a value for the core to admit. Builds green; clippy + `fmt --check`
    clean. Wired (and `ProcessSpawn` deleted) when items 2–3 land.
  - **Landed — the reserved `LOAD_*` exit-status ABI (the child→parent
    contract).** `lib/abi` (`process.rs`) defines the reserved band
    (`LOAD_FAILURE_STATUS_BASE` + `LOAD_NOT_FOUND` / `LOAD_UNVERIFIED` /
    `LOAD_MALFORMED` / `LOAD_OOM`), the total `load_failure_status(Errno)
    -> i32` map the child-load path exits through, and the reverse
    `load_failure_reason(i32) -> Option<&str>` the parent reap turns into
    a `stderr` diagnosis — one definition both sides depend on, with
    round-trip unit tests. The C view (`TAIRIX_LOAD_*` in
    `include/tairix/tairix_syscall.h`) is generated from these by
    `cargo xtask c-header` (drift-guarded); `abi-check` clean. The
    producer (child load path) and consumer (parent reap) are wired when
    the mechanism below lands — until then these are the reserved contract
    the remaining work targets, not a live code path.
  - **Landed — the `(uid, effective, task)` bundle-read authority split
    (Remaining item 2's `load_store_bundle` signature change).**
    `KernelSyscallHandlers::load_store_bundle` and `wait_app_store`
    (`kernel/core/src/syscalls.rs`) no longer borrow a `CallerContext`:
    the bundle read takes its authority as an explicit `(uid: u32,
    effective: &dyn CapabilityQuery)` pair and the app-store park takes an
    explicit `task: u64`. This is the one definition that serves both the
    current caller-side read and the deferred child-side read under the
    child's own credential (`§2.2`), landed live ahead of the flip: the
    sole caller (the synchronous `spawn` handler) passes the caller's own
    `owner()`/`effective()`/`task_id`, so behaviour is unchanged and the
    whole `tairix-kernel-core` suite (1097 host tests) stays green. It is a
    live, in-place refactor, not reserved scaffolding: the child-side call
    site is added by the atomic flip.
  - **Remaining (in dependency order), the atomic mechanism.** The design
    below is validated against the live code (`syscalls.rs` `spawn`
    handler, `KernelSpawnCtx`, `admit_process`, `kthread.rs`, the arch
    producers) and is the contract to build to. The `ProcessSpawn`/admit
    contract ripples to all arch producers + the driver-spawn loader at
    once, so the tree does not build until the whole set lands. Item 1 (the
    arch-neutral seam types) is **landed** — the reserved contract items
    2–7 build on, wired when the deletion of `ProcessSpawn` and the producer
    rewrite land together, exactly as the task-model primitive and the
    `LOAD_*` ABI above are landed ahead of their live use.

    1. **Landed — `BuiltImage` + `ArchImageBuilder` + `ImageBuildCtx` seam
       (`kernel/core/src/spawn.rs`, exported from `lib.rs`).** Builds green;
       `cargo clippy -p tairix-kernel-core` and `cargo fmt --check` clean.
       - `struct BuiltImage { frozen: Box<dyn UserAddressSpace + Send +
         Sync>, physmap: Box<dyn PhysMap + Send + Sync>, stack_span:
         StackSpan, live: Option<Box<dyn LiveUserSpace + Send>>,
         pre_resume: Box<dyn FnMut(u64) + Send>, enter: Box<dyn FnMut() +
         Send> }`. Carries **no** kernel stack: the loading kthread already
         owns the stack it runs on (see 3), and the build only re-expresses
         that stack's guard page in the child's own (inactive) root.
       - `trait ImageBuildCtx` = the build-only subset of `SpawnCtx`:
         `frames() -> &FrameAllocator`, `page_table_allocator() ->
         Option<&'static FrameAllocator>`, `audit() -> &(dyn Sink + Sync)`,
         plus `kernel_stack_guard() -> Option<u64>`. **Refined during
         implementation:** the guard VA is a raw `u64` (the arena stack's
         `guard_page()` returns `u64` and aarch64 `split_block`/`unmap` take
         `u64`), not a `Page`; `None` is the `BoxStack` fallback (self-guards
         with a poison canary, nothing to unmap in the child root).
       - `trait ArchImageBuilder: Send + Sync { fn alloc_kernel_stack(&self,
         frames: &FrameAllocator, pt_frames: Option<&'static
         FrameAllocator>) -> (Box<dyn KernelStack + Send>, Option<u64>); fn
         build(&self, rxe: &[u8], ctx: &dyn ImageBuildCtx, args: &[&[u8]],
         env: &[&[u8]]) -> Result<BuiltImage, Errno>; }`. The arch installs
         one `&'static dyn ArchImageBuilder` (replacing the `ProcessSpawn`
         producer). `alloc_kernel_stack` is the arena/`BoxStack` allocation
         the producer does today, hoisted so the stack exists at admit; it
         returns the stack **and** its guard VA (`Some` arena, `None`
         `BoxStack`), threaded into `ImageBuildCtx::kernel_stack_guard`.
         **Refined during implementation:** `build` drops the `caps`
         parameter — the arch build authorises through the fixed
         `spawn_layout::SpawnAuthority` (holding `CAP_PROC_SPAWN`), never the
         child's own set, and the loading body installs the child's effective
         set directly (item 2), so passing `caps` to `build` would be an
         unused parameter.
       - **Validated:** because the stack is chosen at admit (before the
         child arch space exists), `build` can no longer retroactively fall
         back to `BoxStack` when the guard split/unmap fails — a
         `Some(guard)` whose split+unmap fails in the child root fails the
         `build` **closed** (a stronger guarantee than today's fallback, on
         a path the freshly built identity space makes unreachable in
         practice), never a silent downgrade to an unguarded stack (`§2.17`).
    2. **Owned, `'static` load context + core loading-body orchestration
       (`kernel/core`).** A boot-installed set-once `'static`
       `SpawnServices` handle (the established `devres` / `dispatch_slot`
       / `callreg` `install_*()` / `installed_*()` idiom — a `Once`-guarded
       read-only handle, **not** a mutable global, so it does not offend the
       no-global-mutable-static rule) carries the `'static` load services
       the child body captures: `frames`, `page_table_frames`, `audit`,
       `filesystem`, `app_store`, `aspaces`, `caps`, `process_wait`, and the
       `&'static dyn ArchImageBuilder`. Production backs every one from the
       `Box::leak`'d `KernelState`; tests install a leaked fixture (leak
       permitted in tests). This is preferred over converting
       `KernelSyscallHandlers`'s `'a` fields to `'static`, which would ripple
       through the ~26k-line test suite for no security gain.
       - **Validated: `SpawnServices` is *non-generic*.** The child body's
         only architecture-`A` dependencies are `SchedulerArch::current_cpu`
         (to park on the app-store latch), `ticks_now` (the caps record's
         start time), and the `Clock` the bundle read uses. Type-erase those
         three behind a tiny non-generic `SpawnRuntime: Sync` trait object
         (`current_cpu()`, `ticks_now()`, `now_ns()`), leaked at boot over
         the `KernelState<A>` arch handle. Everything else the body touches
         (`aspaces`/`caps`/`filesystem`/`app_store`/`process_wait`/audit/
         frames/`ArchImageBuilder`) is already non-generic, so `SpawnServices`
         is a plain struct in a plain `OnceCell` — sidestepping Rust's ban on
         generic statics. The one genuinely `A`-generic step, admitting the
         loading kthread (`spawn_kthread_with_stack::<A::Cs, A, …>`), stays in
         `KernelSpawnCtx<A>` where `A` is concrete; the loading body it hands
         the scheduler is `FnMut(&mut Yielder<A::Cs>)` capturing only the
         `&'static SpawnServices`.
       - **Validated: the loading child is a plain kernel kthread**
         (`pre_resume = None`, no live space) until `become_user`, so
         `dispatch_step` publishes it a *body* resume handle and
         `reschedule_current(Park)` suspends it — `wait_app_store` parks
         correctly with no scheduler-fallback dependence in the body.
       - **Validated: a failed loading child's teardown is a complete,
         well-defined subset**, not a partial hack. A child that fails before
         `become_user` provably holds only the admit-time state (caps record,
         aspace entry = streams/limits/cwd/grants, the wait link, procsignal
         gates) — it never bound an IRQ, IPC port, shared-memory region, or
         wait-set. Its teardown is therefore exactly `process_wait.record_exit`
         + caps `remove` + aspaces `withdraw` + `process_wait.parent_exited` +
         the `procsignal` clears, then a plain return from the work body (the
         `kthread` trampoline reports `Exit` and the scheduler reaps it).
         Factor the shared subset out of `reclaim_task_resources` into one
         helper both the full reclaim and the loading-child teardown call
         (`§2.2`), rather than duplicating it or over-tearing subsystems the
         child never touched.
       - The `spawn` handler keeps doing **all** of §2.1 synchronously
         (cap check, path copy-in + `SPAWN_SELF`, attach/streams wiring,
         `resolve_spawn_credential`, `proc_id`/`ProcName`/`spawn_path`,
         program-vs-bundle **syntax** resolution → `NotFound` synchronously).
         It then calls the core admit-loading entry with a `LoadPlan`
         (embedded program bytes+caps, or the parsed bundle path) + the
         per-child data, and returns the minted PID.
       - **Admit-time (synchronous, caller):** allocate the child's kernel
         stack via `ArchImageBuilder::alloc_kernel_stack`; admit a **loading
         kthread** (`spawn_kthread_with_stack`, parked) whose work body
         captures the `'static` `SpawnServices` + `LoadPlan` + per-child
         data. Under the minted `sec_id` install everything that does not
         depend on the manifest: a **loading** caps record (identity =
         proc_id/name/spawn_path/parent/credential/sandbox, **empty**
         capability set), streams + wired std entries, `stack_span` (from the
         validated layout — but see note), inherited limits, cwd, device
         grants + loaded node, and the parent/child wait link; then `unpark`.
         The **user address space is not registered here** (none exists yet)
         — `set_streams`/`set_stack_span`/`install_std_entry` operate on the
         aspaces registry keyed by `sec_id` independently of a registered
         address space, so the streams/limits/cwd install stands. `stack_span`
         is manifest-independent (derived from the layout the build will use)
         — resolve it in the body after the image is built and register it
         there, alongside the frozen aspace, so the caller installs only
         truly manifest-independent state.
       - **Loading body (child task, on its own kernel stack):** in order —
         `wait_app_store` (park, event-woken) → obtain rxe+requested-caps
         (embedded `LoadPlan` yields them directly; bundle `LoadPlan` runs
         `load_store_bundle` under the **child's own** credential = `(uid =
         credential.uid, effective = credential.ceiling)`, the `LaunchCache`
         still hoisting verification off re-launches) → derive effective set
         = `credential.ceiling ∩ requested` and **replace** the loading
         record's empty set under `sec_id` → `ArchImageBuilder::build` →
         register `frozen`+`physmap` and `stack_span` under `sec_id` →
         `yielder.become_user(pre_resume, live)` → `enter()`; caps and aspace
         are installed strictly **before** `become_user`, so the child is
         never dispatchable as a user task under the wrong authority.
       - **`load_store_bundle` signature change — landed.** It now takes an
         explicit `(uid: u32, effective: &dyn CapabilityQuery, task: u64)`
         instead of a borrowed `CallerContext` (`wait_app_store` likewise
         takes an explicit `task: u64`), so the one definition serves the
         child-side read under the child's credential (`§2.2`). The child
         body passes `(uid = credential.uid, effective = credential.ceiling,
         task = child_task_id)`; its `ArchClock`/`audit` come from
         `SpawnServices`. Only the child-side *call site* remains for the
         flip.
    3. **Arch producers + `driver_spawn_loader`
       (`kernel/tairix-kernel/src/{aarch64,riscv64,x86_64}/`).** Each
       existing `spawn_with` body splits: the build portion (parse rxe,
       `user_layout`/`stack_span`, `spawn_image`, freeze, retain `LiveSpace`,
       `pre_resume`, `enter`) becomes `ArchImageBuilder::build` returning a
       `BuiltImage`; the arena/BoxStack allocation becomes
       `alloc_kernel_stack`; the guard-page split+unmap moves into `build`
       driven by `ImageBuildCtx::kernel_stack_guard`. **Validated:** the
       three producers are structurally identical up to the final
       `ctx.admit_process(...)` call (only the arch address-space type, the
       identity-window derivation, and the `pre_resume` body differ), so the
       split is a mechanical extraction with no behavioural change to the
       image build itself. `admit_process` and the
       `ProcessSpawn` trait are **deleted** (the core owns admit now, `§2.14`).
       `driver_spawn_loader` calls the same core admit-loading entry with an
       embedded `LoadPlan` (the verified driver image + granted caps + node
       grants), so a driver spawn defers identically.
    4. **`LOAD_*` wiring.** Child body: on any pre-`become_user` failure,
       audit the load refusal through `SpawnServices.audit` (attributed to
       the child) and `exit(load_failure_status(errno))`. Parent side: the
       kernel reap path (`ProcessWait` / the desktop `CHILD_TOKEN` reap in
       DESK-2) reads the reaped status; the reserved-status → `stderr`
       diagnosis via `load_failure_reason` is DESK-2/DESK-3 (userland) — the
       kernel-side contract (child exits with the reserved status + audit) is
       DESK-1.
    5. **Tests (host).** admit returns a PID while the bundle bytes are
       untouched (an instrumented FS records zero reads until the child
       runs); missing/tampered/malformed/OOM each admit then exit with the
       matching reserved `LOAD_*` status + audit event; a valid embedded and
       a valid bundle load and reach `enter`; credential is resolved
       synchronously and the child reads under it. Per-arch QEMU verticals:
       a session-spawned app loads and runs; a bad path admits then exits
       observably; existing spawn/session verticals still pass.
    6. The full `§7` whole-project gate green.
- **DESK-2 … DESK-7 — planned.**

---

## 6. Cross-references

- `plans/FIX-SYSCALL.md` — P-5b (syscalls run with IRQs enabled; the
  kernel stays non-preemptible per task) is *why* a long syscall body on
  the desktop's task freezes the desktop even though the machine is not
  wedged. This plan removes the long body from the caller's task.
- `plans/SPAWN.md` — the `SPAWN` syscall, admit path, and parent/child
  wait link this plan defers the load behind.
- `plans/APPS.md`, `§16.5` — app bundles are loaded from disk through
  `appmgr`/the load gate; the deferral does not change *what* is loaded
  or verified, only *which task* runs the load.
- `plans/APPWIN.md`, `plans/DISPLAY.md` — the desktop window/present
  path whose loop must never freeze.
- `kernel/mem/src/filemap.rs`, `anon.rs`, `reclaim.rs`, `pressure.rs`,
  `spawn.rs`, `loader.rs` — the existing demand-page fault path,
  fresh-frame discipline, reclaim/pressure machinery, and image build
  that §2.6 builds the verified shared image on; `kernel/core`'s
  `launch_cache.rs` (the `LaunchCache` folded into that cache) and
  `lib/appload/src/loader.rs` (the `content_hash` the cache is keyed on).
- `AGENTS.md` `§2.16`, `§2.23`, `§5.4`, `§17.2`, `§19`, `§24`, `§24.1`,
  `§26.1`, `§26.2`, `§26.3`, `§26.6`, `§26.7` — the rules this plan
  enforces (including the demand-paged, CoW-shared, verified image of
  §2.6, which puts program loading strictly ahead of Linux).
