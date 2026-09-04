# FIX-DESKTOP.md — Non-blocking desktop: asynchronous process launch

Binding under `AGENTS.md`. This plan removes the desktop freeze that
occurs while an application is loaded and started, fixes it as a
first-class property of the process-launch path (not a desktop-local
band-aid), and stages the identical fix for every other interactive
loop that inherits the same defect.

The rule this plan enforces is now stated outright in the charter (`§28`,
"Interactive Surfaces Answer Within a Frame"): an interactive surface neither
waits nor works unboundedly per event — it performs no blocking I/O, and it
updates state on input and paints once from that state, scoped to what changed.
This plan is that rule's staged enforcement across the desktop; it is no longer
where the rule is derived.

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
| Desktop launcher | `userland/gui/session/src/run.rs` (the taskbar-launch arms — today the program-library popup and its row activations — `tairix_rt::spawn(...)`) | Compositor loop frozen for the whole launch. **The reported bug.** |
| Desktop file picker | `userland/gui/session/src/run.rs` (the `SessionPicker`'s `VfsDirectorySource` calling `tairix_rt::read_dir_all` inside `picker.handle_click` / `handle_key`, on the `SEAT_TOKEN` path) | Compositor loop frozen while a directory is read from disk on every open/navigate. Same class of defect (synchronous I/O on the compositor thread), smaller blast radius. |
| Desktop icon artwork | `userland/gui/session/src/run.rs` (every icon surface resolving through `tairix_icon::ArtworkCache` *inside* its paint) | Compositor loop frozen for a bounded read plus a sandbox round trip, per icon, the first time each is drawn at a given pixel side. Worst on bring-up and on opening the launcher. Fixed in DESK-8. |
| Desktop program catalog | `userland/gui/session/src/run.rs` (`refresh_library` + `desktop_associations`, on the `OpenLibrary` arm and after a re-list) | Compositor loop frozen while the two catalog documents and then **one `AppInfo` per catalogued application** are read — on the very click that opens the launcher. Fixed in DESK-9. |
| Desktop settings publish | `userland/gui/session/src/run.rs` (`adopt_pinboard_settings` → `persist_pinboard`) | Compositor loop frozen for a store open + publish + reload, *deliberately before* the change was adopted. Fixed in DESK-10. |
| Terminal settings sheet — the write | `userland/apps/terminal/src/{settings,run}.rs` | One store read + publish + reload **per pointer-motion sample of a slider drag**, on the terminal's own loop. Fixed in DESK-10. |
| Terminal settings sheet — the redraw | `userland/apps/terminal/src/run.rs` (`redraw_windows`) | With the write gone, **every sample still re-derived the whole look**: it discarded the retained screen, re-ran the effect pipeline over every pixel, reallocated the scratch surface, resized the pty, and reset the persistence trail — whichever field moved. Fixed in DESK-13. |
| Wallpaper chooser *Apply* | `userland/apps/wallpaper/src/run.rs` (`apply`) | A synchronous `PINBOARD_ENDPOINT` `ipc_call` on the chooser's loop, answered only once the session's publisher has written the store: the window froze for a disk commit. Fixed in DESK-14. |
| File manager reader wake | `userland/apps/files/src/run.rs` (`RtEventSource::park`) | The reader's wake was added to the wait-set but never drained, and `park` reported it as "no event yet". A wait-set stream member is a **level** peek, so after every answered listing the park spun at 100% until the next input event happened to break it. Found by DESK-14; fixed there. |
| File manager listings | `userland/apps/files/src/run.rs` (`LiveSource`) | Window frozen for every directory read: navigation, reload, and the bring-up open. Fixed in DESK-11. |
| File manager folder cues | `userland/apps/files/src/run.rs` (`resolve_occupancy` **inside the render**) | `open_dir` + `read` + `close` per newly-visible folder, while painting. Fixed in DESK-11. |
| File manager "Open With…" | `userland/apps/files/src/run.rs` (`RtBundleSource`) | Three whole program stores walked, one `AppInfo` per bundle, on the click that opened the chooser. Fixed in DESK-11. |
| File manager icon artwork | `userland/apps/files/src/icons.rs` | One bounded read plus a sandbox round trip **on the event loop**, once per turn. Already outside the paint and interleaved with input service, but still I/O the loop performs. Fixed in DESK-12. |
| Statistics reported to `sysinfod` | `lib/rt/src/cachereport.rs`, `userland/gui/session/src/frames.rs` | Up to **eight blocking cross-process round trips a second** on the compositor's own frame path, and on the file manager's and `fontd`'s loops, purely to report counters. `ipc_call` parks the caller off the run queue, so a gesture stuttered four times a second and every app blocked in a window call waited behind it. Fixed in DESK-15. |
| Terminal settings sheet — the sheet's own pixels | `userland/apps/terminal/src/run.rs` (`present_overlay`) | The damage its controls reported was **computed and discarded**: every pointer sample allocated a sheet-sized surface, re-rendered every tab, row, label and swatch, and presented the whole popup. Fixed in DESK-16. |
| Desktop listing + wallpaper workers | `lib/browse/src/desk.rs`, `userland/gui/session/src/wallpaper.rs` | A **runaway**: the job hand-out cloned the request instead of taking it, so an answered job was immediately workable again and the worker re-ran it for ever — 1030 directory reads of one folder in 13 s, ~150/s, each waking the compositor. A core and a disk spent continuously, contending with every frame. Fixed in DESK-17. |
| Compositor glyph misses | `userland/gui/session/src/run.rs` (text drawn on the frame path through `lib/font`) | A glyph the client cache did not hold was a **blocking `FONT_ENDPOINT` round trip on the compositor loop** — 41 of them in a 13 s hover run, clustered where new text appears: 32 inside 41 ms on window-open. The cache absorbs the steady state, so this was a cold-cache stall rather than a periodic one. Fixed in DESK-18. |
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
- Store-bundle **existence** probe: for a well-formed bundle path, one
  cheap directory-metadata lookup of the bundle root (`stat`), under the
  same `(uid, effective)` the deferred load reads with
  (`SpawnCredential::read_authority`), confirms a bundle is actually
  there before a loading child is admitted. This keeps the handler's
  `NotFound`-for-an-unknown-path contract: **absent** → `NotFound`
  synchronously (so a caller's ordered command search advances to the
  next candidate, `plans/APPS.md` §8); **present but the caller may not
  read it, or any other non-absence error** → admit the child (so the
  search *stops* and the deferred load surfaces the real refusal via the
  child's exit — a missing bundle a later user-writable candidate could
  shadow must never be silently skipped). With no store installed the
  probe fails closed with `NotFound` at once; while the `/System` mount
  is still pending the child is admitted to *park* on the store latch
  (the answer is unknowable without racing the mount, and the parent must
  not block); a bundle the launch cache already holds skips the lookup
  (already proven present this boot). Only this one metadata lookup is
  synchronous — a search inherently pays one lookup per candidate, like a
  POSIX `PATH` walk; the bundle read, content hash, signature verify, and
  `rxe` decode all stay deferred (§2.2).
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
  `LaunchCache` still hoisting verification off re-launches). Only the
  *existence* of the bundle root is confirmed synchronously (§2.1's cheap
  metadata probe); the megabyte read + cryptography this bullet covers is
  what must stay off the caller. The VFS read is re-authorised under the
  child's own kernel-attested credential (`§5.4`), which is *more*
  correct than authorising under the caller.
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
    `(uid, effective_caps)` pair rather than a borrowed `CallerContext`.
    That pair comes from one shared source, `SpawnCredential::read_authority`,
    which both the child-side read here and the synchronous existence probe
    (§2.1) call, so the probe and the load can never judge a bundle
    present-or-absent differently — one definition, no fork (`§2.2`).
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
- **Spawning on a second desktop thread.** Rejected: it would still block *a*
  desktop task on I/O, and every other caller of `spawn` would keep the freeze.
  The defect belongs to the launch path, so that is where it is fixed — and once
  fixed there, no caller needs a thread for it. (Userland does now have threads,
  `plans/THREADS.md`; the rejection stands on its own merits and never rested on
  their absence.)

### 2.5 The picker listing and the wallpaper (same defect, same principle)

A directory listing (`read_dir_all`) and the wallpaper's read-and-decode both
ran on the compositor thread. The first-class fix is symmetric: work that backs
an interactive UI and takes as long as the hardware takes must not block the UI
loop.

The resolution taken in DESK-4 is the asynchronous one: the session requests the
work, a `lib/rt` worker thread does it, and the answer arrives through the
wait-set the session already parks in. It keeps the picker's capability
discipline (the session lists under its own authority; the app lists nothing) and
its fail-closed behaviour — a refused listing leaves the view exactly where it
was, never a guess — and it gives the view a real pending state rather than a
silent stall. Details in DESK-4 (§4).

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
- **Deliverables:** `userland/gui/session/src/run.rs` launcher arms return
  immediately (they issue `spawn`, which now only *admits* the child), and
  a child that exits with a reserved `LOAD_*` status is reported on `stderr`
  (`§24.1`) named by its launcher label instead of vanishing silently. The
  reserved-status → message mapping is `lib/abi::load_failure_reason` (the
  one shared definition, `§2.2`); the desktop remembers each launched
  child's label (`in_flight` map) and the whole reap-and-report flow is the
  host-tested `reap_launched` in `userland/gui/session/src/launch.rs`.
- **Tests:** the reap-and-report path is host-tested end to end
  (`launch::tests`): a scripted multi-child drain reports only the reserved
  refusal (named), a clean exit reports nothing, an unrecorded child is
  still reported under a fallback label, every reaped PID reaches teardown,
  and the decision function maps every reserved status / ignores every
  ordinary exit or stop. The *responsiveness* half needs no bespoke display
  vertical: the compositor loop presents once per wait-set wake and no
  longer blocks on `spawn` at all — that is DESK-1's admit-then-load, which
  the per-arch spawn verticals already prove (admit returns a PID without
  touching bundle bytes; a bad bundle admits then exits with the reserved
  status). Reusing those proofs instead of standing up a display/seat/input
  QEMU harness purely to scrape a `stderr` string avoids disproportionate
  test scaffolding (`§2.3`) while covering the actual DESK-2 logic.

### DESK-3 — Same fix, other interactive loops
- **Deliverables:** confirm `elsh` foreground launch and the terminal
  startup benefit with no code change (they already reap), and add the
  load-failure diagnosis where a caller previously relied on a
  synchronous `-errno` for an I/O/verification failure. Update
  `plans/SHELL.md` / `plans/APPS.md` cross-refs if the launch-failure
  reporting wording changes.
- **Tests:** shell reports a failed foreground launch with its reason;
  job control stays responsive during a load.

### DESK-4 — Directory listings and the wallpaper off the loop
- **Done.** Both reads that can take arbitrary time on arbitrary hardware now
  run on `lib/rt` worker threads, woken back into the session through one byte
  on a pipe whose read end is a `WaitSourceKind::Stream` wait-set member — no
  new ABI, no second wake mechanism, nothing spinning.
- **The shape, twice.** A *desk* is the host-tested policy — what each consumer
  asked for, what has come back, the staleness rule that discards an answer for
  somewhere the desktop has since left — with no lock, thread, or syscall in it:
  `ListingDesk` (`lib/browse/src/desk.rs`) and `WallpaperDesk`
  (`wallpaper.rs`). The `Run` binary adds the runtime's futex mutex, a condition
  variable the worker parks on, and the shared `WorkerWake`.
- **Listings.** Two consumers — the desktop icon column and the trusted picker —
  are *named*, not counted (a structural fact of the session, not a capacity),
  with a slot each and round-robin service, so a picker walking a deep tree
  cannot hold the icon column's re-list behind it.
- **The `DirectorySource` contract** (escalation 1, resolved) now answers
  `Listing::Ready | Listing::Pending`, `Err` being a third thing entirely.
  `Browser` records a pending navigation and **moves nothing** until
  `Browser::resume` commits it — location, entries, and both histories are
  untouched — so the transactional fail-closed guarantee is exactly what it was
  and a refusal is reported in place. The renderer draws `Listing…` in the
  listing area while a read of *somewhere else* is in flight (the items on
  screen belong to a directory the user asked to leave); a re-read of what is
  already shown keeps its items, so a periodic re-list cannot flicker.
- **The wallpaper's sandbox** (escalation 2, resolved as a *second* worker): the
  icon rasteriser keeps the serve loop's own handle, untouched and deliberately
  not `Send`; the wallpaper thread creates its own capability-empty worker
  inside itself, so no sandbox handle crosses a thread boundary.
- **Degradation.** A refused pipe or a refused thread is stated once and that
  work happens on the serve loop, exactly where it used to be.
- **Not in scope, and why.** `DirectorySource::has_children` (the optional
  occupancy cue) stays synchronous: it is a bounded one-record read, it is
  already permitted to answer `NotImplemented`, and neither session consumer
  asks for it — the desktop icon column and the picker both take the default.
  The file manager, which does opt in, blocks only its own loop.
- **Tests:** the desk policies host-tested beside their code (handshake,
  dedup, staleness, round-robin fairness, stop); `lib/browse`'s `deferred`
  suite drives a `Browser` end to end over a never-ready source (pending open,
  resume, nothing moves early, second navigation replaces the first, refusal
  leaves the view put, back/forward commit their own history move, a pending
  reload keeps its items); the cue's two render cases.

### DESK-8 — Icon artwork off the loop
- **Done.** Resolving one icon costs a bounded VFS read plus a round trip to
  the parser sandbox that decodes it, and every icon surface — the bar's pins
  and task slots, the launcher popup's rows, a window's title-bar identity, the
  desktop's own column — used to pay it *inside the paint*, on the serve loop. A
  launcher opening on thirty applications paid it thirty times before its first
  pixel. It now runs on a third `lib/rt` worker thread, woken back through the
  same `WorkerWake` pipe DESK-4 built.
- **The seam is in `lib/icon`, not the session.** `ArtworkResolver` separates
  *deciding what a draw needs* from *producing it*: `InlineArtwork` reads and
  decodes on the calling thread (what a session the kernel granted no thread
  falls back to), and `ArtworkDesk` answers `Resolved::Pending` until the
  pixels land. Both produce the decode through the one `render_artwork`, so
  where it ran cannot change the result. A
  pending tier **stops** the tier walk rather than falling through, because
  whether a later tier is reached depends on this one's answer — so a deferred
  request costs exactly the reads a synchronous walk would, spread over as many
  answers as it has tiers.
- **The desk** is `tairix_icon::ArtworkDesk` (`lib/icon/src/desk.rs`), the same
  shape DESK-4 established: host-tested policy with no lock, thread, or
  syscall; the `Run` binary adds the futex mutex, the condition variable the
  worker parks on, and the shared wake. It lives in `lib/icon` beside the
  `ArtworkResolver` contract it implements — and *is* that resolver — because
  the file manager drives the same desk from its own reader thread (DESK-12,
  `plans/NEW-FILEMANAGER.md`) and `userland/apps/*` may not depend on
  `userland/gui/*`. One policy, one drive mechanism: a worker thread behind a
  futex mutex in both processes.
- **What the desk remembers.** An answer handed over is forgotten: the cache
  owns it, so if the cache later evicts it the next paint's miss is genuine and
  is decoded again. The decode cache is budgeted, though, so it can be asked to
  hold more than it will, and a decode it *refuses* must not be offered again —
  the repaint its landing drove would ask, the answer would be refused again,
  for ever. The cache reports that refusal and the desk holds the key declined
  until the pressure band moves.
- **One repaint per batch.** The worker nudges when its queue drains rather than
  after each icon, so a bring-up wanting thirty of them costs the desktop one
  repaint and they appear together; a lone icon empties the queue at once and
  still lands the moment it is ready.
- **Asked for early, not on the frame that needs it.** Moving the decode off the
  loop removes the freeze but not the round trip, so a surface that first asks as
  it *paints* shows a screenful of built-in glyphs and fills in one icon at a
  time afterwards — which is exactly how it presented: a launcher opening on
  generic pictures, a pinned application wearing its fallback, a window opening
  under the shared application glyph. The desktop therefore **warms** what it
  knows it will draw the moment it knows it: `ArtworkResolver::prefetch` and
  `ArtworkCache::prefetch` are the seam (one tier — the first not already held —
  asked for, nothing drawn, nothing waited on), `ArtworkDesk::want` is its half
  of the desk, and the session drives it from the three places the set can
  change: a catalog read, a pin re-resolve (`Taskbar::catalog_icon_wants`, the
  popup's first screenful of rows plus every pinned application, sized from the
  same layout the paint uses), and the launch table (`warm_launched_artwork`, at
  the two sides a window wears — a spawn, a load, and the app's own bring-up
  before there is a window to put one on). An icon already held asks for
  nothing, and the inline resolver prefetches nothing at all, so a session with
  no decoder thread is unchanged.
- **One definition of the title-band icon side.** Warming a window's identity
  needs the side *before* the window exists, and the band's height is the
  theme's rather than any one window's — so `WindowFrame::identity_icon_side`
  now states it and `Compositor::window_title_icon_side` reads it, replacing the
  per-window frame-layout derivation it used to reconstruct. `id` decides only
  whether there is an identity slot, never how big it is.
- **Adopting a landing.** Most surfaces adopt by asking the cache again, which a
  repaint does. The two that *store* the picture — the pin strip and a window's
  title-bar/taskbar identity — are offered it again explicitly on the wake;
  `ArtworkCache::owned_artwork` answers those callers `Ready`/`Refused`/`Pending`
  so a refusal leaves the identification list and only a genuine wait stays on
  it. That call also hands back a decode the cache was too tight to retain,
  which the borrow-returning path could only throw away.
- **A duplicate deleted on the way** (`§2.2`): the window-identity path had
  hand-rolled the bundle tier (read the manifest, decode the header, derive the
  asset, resolve *that*) beside `lib/icon`'s own. It now states
  `IconRequest::bundle` like every other surface, so the second window of an
  application costs no read at all rather than re-reading its manifest.
- **The worker owns its own sandbox**, as the wallpaper worker does, so no
  sandbox handle crosses a thread. On a desktop that got its threads the serve
  loop's own sandbox child is never even spawned.
- **Degradation.** A refused pipe or a refused thread is stated once and the
  decode happens on the serve loop through `InlineArtwork`, exactly where it
  used to be.
- **Tests:** the desk policy host-tested beside its code (handshake, dedup by
  key *and* pixel side, hand-out order, a dropped answer decoded again against a
  refused one held, a stale queue entry yielding no duplicate, teardown); `lib/icon`'s deferred suite (a pending
  decode retains nothing, a landed one is served and retained, a pending tier
  stops the walk, a landed refusal advances it one tier at a time,
  `owned_artwork` telling `Pending` from `Refused`, and a picture handed back
  from a cache under pressure); and the session's identity pair — a window
  pending at open is pictured when the decode lands and then leaves the list, a
  window whose application has no picture leaves it immediately.

### DESK-9 — The program catalog off the loop
- **Done.** The catalogue and the file-type associations are one snapshot read
  on a worker (`load_programs`, `userland/gui/session/src/library.rs`): the two
  layers merged as before, then one `AppInfo` per catalogued application, all
  fail-closed per layer and per bundle. The `Catalogs` desk is a
  `tairix_util::defer::JobDesk` — one scan waiting, one in flight, latest-wins —
  and the `OpenLibrary` click submits and opens the popup on the catalogue
  already in hand, adopting the fresh one on the wake it nudges. A re-list
  submits the same job, coalesced.
- **No popup pending state was needed after all.** The stage anticipated one,
  but the popup opening on the catalogue it *has* is both simpler and better:
  the launcher opens instantly on what the user last saw and updates in place,
  where an empty-popup-that-fills would have shown nothing on the very gesture
  that asked. A fresh session's first popup is the only case with nothing to
  show, and its catalogue is read at bring-up on the session's own task, before
  any window is on screen.
- **One snapshot, not two reads.** The associations come from the bundles the
  *same* scan catalogued, so a click can never resolve a bundle against a
  catalogue it was not read from — which the two separate reads it replaced
  could.
- Tests: `load_programs_reads_one_manifest_per_catalogued_bundle`
  (`userland/gui/session/src/tests.rs`) — one read per catalogued bundle, an
  unreadable manifest claiming nothing.

### DESK-10 — Settings writes off the loop (the reported slider)
- **Done.** A control's value is no longer tied to a store write anywhere on
  the desktop.
  - **`lib/controls`** gives a continuous control a settle point:
    `SliderAction::Settled` alongside `SetValue`, reported by the release that
    ends a drag (and at once by a key step, which is one whole interaction).
    Durable work belongs on the settle; acting on every value change is acting
    once per pointer sample.
  - **The terminal** separates the profile the windows *render* from the one the
    store *holds* (`tairix_terminal::publish::Publication`). A drag previews
    every sample and writes nothing; the settle asks the settings worker for one
    write; the answer — what the store then implies, so a machine policy still
    wins — becomes the profile in force. A refused write reverts the preview and
    states why, so no window keeps showing a look the next start would not
    restore. *Restore defaults* is the same path with no second mechanism.
  - **The desktop session** keeps persist-then-adopt and moves it off the serve
    loop: both routes into the pinboard settings submit to the `Publisher` desk
    and adopt nothing, and the answer is adopted on the wake. The chooser's
    `PINBOARD_ENDPOINT` call is answered when the store has spoken, so it still
    reports whether its document was published; a request the next gesture
    overtakes before any worker took it is answered there and then.
- Tests: `dragging_the_text_size_settles_once_however_many_samples_it_takes`
  and `clicking_the_text_size_track_settles_the_value_it_jumped_to`
  (`userland/apps/terminal/src/settings_tests.rs`),
  `a_drag_reports_one_settle_however_many_samples_it_took`
  (`lib/controls/src/value_tests.rs`), the `publish` suite (the live/adopted
  pair, the store's answer winning, a refusal reverting), and the `defer` suite
  (`lib/util/src/defer_tests.rs`).

### DESK-13 — Redraws scoped to what changed (the reported slider, again)
- **Done.** A control's value no longer drives an unscoped rebuild. DESK-10 took
  the *write* off the drag; this takes the *work* off it.
  - **`Invalidation`** (`userland/apps/terminal/src/profile.rs`) is the set of
    kinds one profile change makes stale — cell metrics, resolved colours,
    effect passes, backdrop-blur radius — derived by comparing the profile in
    force with the one replacing it. It is a set rather than four bools so no
    caller can build one positionally and transpose two kinds.
  - **`Publication` measures what the screen owes**, not what each edit asked
    for: it holds the profile it last *rendered* beside the live and adopted
    ones, and `take_pending` answers the difference and records the catch-up.
    Diffing against the screen is what makes a burst of previews cost one
    paint, and is why no preview can be stranded by an outcome that lost.
  - **The terminal does only the stale work.** Blur is one bounded message to
    the compositor and no pixels of its own; a size change is the only one that
    re-fits the face, re-derives the grid, and resizes the pty; colours and
    metrics are the only ones that discard the retained screen and the
    persistence trail. The trail surviving an effect slider is a correctness
    fix, not only a speed one — resetting it flickered the persistence away on
    every sample of its own slider. The blur diff is taken on the **radius**,
    so the thousand-step slider's samples that land on a width the compositor
    already shows send nothing at all.
  - **The sheet's own pixels are a separate question** (`Sheets`), because the
    knob moves at a permille the window cannot see: answering only the window's
    would freeze the slider under the pointer.
  - **The loop paints from state.** An unsettled edit folds into the drain and
    concludes it once; `apply_outcome` then brings the screen up to date with
    whatever an outcome previewed and did not draw. A settle still concludes
    the drain at once — it is one event per gesture and it owes a write.
  - **`DesktopChanged` collapsed onto `restyle_windows`**, the whole-surface
    path a re-theme or scale change genuinely needs, removing a near-copy that
    had also been failing to re-present an open sheet at the new scale.
- Tests: the `Invalidation` suite (`profile_tests.rs`) — a transparency change
  staling only the colours, a blur too fine to see staling nothing, a visible
  blur repainting nothing here, going opaque withdrawing the blur, only a size
  change reaching the metrics, each effect strength staling only the passes;
  and the owed-paint suite (`publish_tests.rs`) — a burst owing one paint, an
  undrawn preview staying owed, adopt and a refusal owing the difference from
  the screen.

### DESK-14 — The wallpaper chooser's *Apply* off its loop
- **Done.** The chooser's click no longer waits on the session's store write,
  and the arrangement that takes such work is now one shared thing rather than
  a copy per app.
  - **`tairix_rt::work`** is the runtime half of `tairix_util::defer`: the
    `JobDesk` supplies the bookkeeping, and `Worker` supplies the exclusion,
    the parked thread, the wake, and the fall-back. The work is a plain
    `fn(&Req) -> Ans`, so the worker thread and the fall-back path cannot be
    given two that disagree, and nothing is boxed. The terminal's `Publisher`
    is now a type alias over it — the local copy is gone, not duplicated.
  - **The fall-back is one path, not a second one.** A refused wake pipe or
    thread leaves the desk stopped, which makes a later `submit` carry the job
    out on the caller's thread and leave the answer where `collect` looks,
    answering `true` so the caller collects at once. `NoWorker` says which
    refusal it was, so each app words its own message.
  - **The chooser** encodes the document on the loop (in memory, refusable on
    the spot) and submits only the round trip. `ApplyOutcome::Applying` is what
    the footer shows meanwhile, so it can never report a result the store has
    not given.
- **A wait can now end without an event.** `EventSource::park` answers
  `Parked::{Served, Interrupted}` and `WindowEvents::wait` answers
  `Option<WindowEvent>`. This is what a worker's answer needs: a wait-set
  stream member reports **buffered bytes, not an edge**, so a wake the source
  treats as "no event yet" is a source that is *still* ready — the park then
  spins rather than waits, and the answer never reaches the loop.
  - **This closed a live busy-spin in the file manager.** Its reader wake was
    on the wait-set but never drained, and `park` reported it as nothing, so
    every answered listing spun a core until the next input event happened to
    break it. Tests masked it because a vertical's next keystroke always
    arrived. It drains and interrupts now, like the chooser.
  - The three apps that park on nothing of their own (`viewer`, `widgets`,
    `datetime`) answer `Parked::Served` and go round again.
- Tests: the `work` suite (`lib/rt/src/work.rs`) — a stopped worker running the
  job on its caller and leaving the answer to collect, a running one deferring
  it, the guard stopping what it holds, and a start with no wake leaving the
  work on the caller; and the interrupted-park pair
  (`lib/window/src/tests.rs`) — a wait ending with no event, and parking once
  rather than spinning.

### DESK-15 — Statistics are handed over, never awaited

- **Done.** Two rate-limited self-reports to `sysinfod` — the reclaimable-cache
  ledgers (`lib/rt::cachereport`) and the compositor's cumulative frame
  accounting (`session::frames`) — were made with `ipc_call`, which parks the
  calling task off the run queue until the service replies. They sit at the end
  of the desktop compositor's own wake, and on the file manager's and `fontd`'s
  loops, so a gesture paid up to eight cross-process round trips a second for
  counters nobody was waiting on.
- **What it cost, measured.** The aarch64 hover vertical, instrumented to time
  each publish and each phase of a wake: the cache report reached **33.6 ms**
  and the frame report **10.9 ms**, against a typical whole wake of 1.8 ms
  during the sweep. Seven sysinfo round trips landed inside the one bracketed
  second of the gesture. The debug guest's serial-logged kernel inflates every
  absolute figure, so the load-bearing reading is the *shape*: a publish costs
  multiples of a frame and recurs four times a second per publisher, whatever
  the machine.
- **The fix is the asynchronous call ABI, which already existed.**
  `tairix_rt::submit::Submission` posts the request (`call_post`), the loop
  carries on, and the verdict is reaped without blocking (`call_reap`) on the
  pass the rate limiter already armed a wake for. One submission is outstanding
  at a time and each carries the publisher's own interval as its deadline, so a
  wedged service costs one restated figure rather than a blocked loop. No new
  syscall, no ABI change, no worker thread, no capability: `sysinfod` is
  untouched and the wire request is the one it already served.
- **The verdict is what decides the figure**, so the gate still only ever
  believes the service holds what it accepted: a refusal drops the figures it
  carried and restates them. The **withdrawal** is the one report that still
  waits, because the kernel drops a posted request whose poster has exited
  (`CALL_POSTER_VANISHED`) and a lost withdrawal leaves a monitor showing
  memory nobody holds; it abandons any outstanding report first so nothing it
  carried can land after and resurrect the rows.
- The file manager and `fontd` needed no change of their own: both drive
  `cachereport::publish_if_due`, which is where the shape lives.

### DESK-16 — The settings sheet repaints what its controls reported

- **Done.** The sheet's controls were already reporting the rectangles they
  change into the shared damage sink; `present_overlay` created the sink,
  filled it, and dropped it — then allocated a sheet-sized surface, rendered
  the whole sheet, and presented the whole popup. So the reported slider drag
  cost the whole sheet per pointer sample, on top of the whole-window
  recomposite the transparency and blur fields legitimately ask the compositor
  for.
- `terminal::sheet::SheetScreen` is the sheet's retained picture, the sibling
  of `render::Screen` for the grid: the reports accumulate in it across a
  drained batch, `paint` clips the render to them and answers the rectangle,
  and `write_frame` and the present carry exactly that. `invalidate` covers the
  sheet for a change no control could have reported — a re-theme, a new scale,
  a profile adopted from the store, or a released frame region, which holds
  none of the pixels a partial present would leave standing.
- Tested from both ends so neither half can pass by covering everything: a
  scoped paint is byte-identical to the same band of a whole one, and the
  effect sliders' own reports are asserted to be a small part of the sheet.

### DESK-17 — A desk hands a job out by taking it, never by copying it

- **Done.** Two hand-rolled desks handed a worker its job by *cloning* the
  request and leaving it standing: `tairix_browse::ListingDesk` (the desktop's
  icon column and the trusted file picker) and the session's `WallpaperDesk`.
  `next_job` set the in-flight flag and returned a clone; `deliver` cleared the
  flag and stored the answer but left the request — so `has_work()` was true
  again the instant the job was answered, and the serve loop went straight
  round and ran the *same* job again. For ever.
- **What that cost, measured.** In the aarch64 hover vertical the listing
  worker made **1030 `fs_open` and 1026 `fs_write`** calls and *no other
  syscall at all* — one directory read of the user's `Desktop` folder every
  ~6 ms, about 150 a second, each completion nudging the compositor awake to
  re-check a frame. On a single-core machine that is a core spent, a disk kept
  busy, and the compositor woken continuously; the wallpaper desk's loop is the
  same shape but each turn reads, decodes and resamples a whole screen.
  This is the busy-poll the charter forbids, reached through a worker rather
  than a `yield` loop.
- **The fix is the invariant both slots already documented** — "cleared when
  its answer is stored" — which only `deliver` failed to honour: an accepted
  answer clears the request it answers. The consumer-side clear in `take` went
  with it, because that is where the invariant used to be enforced *by
  accident*: the worker checks `has_work()` long before the consumer ever calls
  `take`, so the flag has to be right at delivery.
- **Why only these two.** Every desk that hands out by *taking* was always
  correct: `tairix_util::defer::JobDesk` (`pending.take()`), `ArtworkDesk` (a
  `Wanted → Running → Done` state, and only `Wanted` is handed out — which is
  why the same run decoded exactly 12 icons, each key once), and the file
  manager's probe desk (`mem::take` of its batch). The defect is precisely the
  clone-and-leave hand-out.
- Both fixes carry a regression test that fails before and passes after, in the
  crate that owns each desk, plus one pinning the staleness rule the fix must
  not break (an abandoned answer leaves the *newer* request standing).

### DESK-18 — The font protocol is per run, not per glyph

- **Done.** Text is drawn on the frame path, and a glyph the client cache did
  not hold was a synchronous `FONT_ENDPOINT` call to `fontd` — 41 of them in a
  13-second hover run, clustered where new text appeared rather than spread:
  **32 calls inside 41 ms** on window-open. The client cache absorbs the
  steady state, so this was the cold-cache stall (a window, a launcher, a new
  label) rather than a periodic one, but each was a full cross-process round
  trip on a loop that owed the user a frame.
- **Why neither cheaper fix works.** It cannot be deferred the way a read can:
  a paint that draws nothing where a letter belongs is a wrong frame, and the
  only placeholder for a letter is the console atlas, whose advances differ
  from a real face — so a placeholder run would visibly reflow when the styled
  glyphs landed. A same-thread pre-warm is not a fix either: asking before the
  paint moves the round trip earlier in the *same wake* and removes no stall.
  The round trip itself is inherent, because sandboxing the rasteriser is what
  puts the faces in another process. So the goal is one per run, converging to
  none once warm.
- **The fix is the protocol.** `FontRequest::Glyph` became `Glyphs`, carrying a
  bounded inline `GlyphRun` (1..=`FONT_MAX_GLYPH_RUN` = 32, the measured burst
  exactly) in a request that stays one fixed 164-byte length with every unused
  field and run slot validated zero. The reply became a batch: the service
  appends records until the next will not fit and states how many it answered,
  in order, and the client asks again for the remainder. No bound moved
  (§24.4) — `FONT_MAX_GLYPH_REPLY` is re-derived as the batch header plus one
  widest, tallest record, over the same untouched coverage bounds — and a
  successful batch always answers at least one glyph, so a client walking a run
  always progresses. The full protocol statement is `plans/FONT-SERVICE.md`
  §2.2/§2.3.
- **One warm step serves both entry points.** `FontClient::warm` scans a run
  for what the cache does not hold and fetches it a bounded run at a time;
  `measure_text` (which drives `text_width`, `truncate_to_width` and
  `elide_to_width`) and `draw_text` both go through it, because both missed per
  glyph. There is no second cache — the coverage fetched while measuring is
  what the draw blits — and the scan peeks, so recency stays the drawing
  lookup's. Warming is skipped with no cache installed and abandoned after one
  batch the cache kept nothing of, so a client whose cache admits nothing
  (`plans/FONT-SERVICE.md` §3.2) pays no batch on top of the per-glyph call it
  already pays.
- Tested as counts, never timings: a cold run is asserted to cost one round
  trip and 40 distinct scalars two, an uncached client is asserted to pay no
  batch at all, and a run too large for one frame is asserted to page and
  still end with every glyph resident. Output is bit-identical — the existing
  blit-reference tests are unchanged.

### DESK-11 — The file manager's reads off its loop
- **Done.** Every unbounded read the file manager makes now runs on one reader
  thread, and the window keeps drawing throughout. The three kinds share a
  worker rather than taking one each: the app browses one place at a time, so
  they are never concurrent workloads, and one worker gives the order they are
  served in a single stated answer — the listing first (the user navigated and
  is waiting), then the folder cues (which decorate a listing already shown),
  then the bundle scan (which no frame depends on). Nothing starves: each
  request set is finite and refilled only by the user asking again.
  - **Listings** go through `ListingDesk<FilesClient>` and a
    `DirectorySource` that records and answers `Listing::Pending`;
    `Browser::resume` commits the answer on the wake. The one read left on the
    app's own task is `first_listable`, which runs before any window exists and
    answers *which location to open* — a question a deferred source cannot
    answer, since its first answer is always "not yet" and every candidate would
    look listable.
  - **Folder cues** gained a "not yet" (`tairix_browse::Probe`), so
    `Browser::resolve_occupancy` may be called from inside a paint without the
    paint performing any I/O: the ask records, the answer is drawn a frame
    later. The recorded set is probed as one batch, because a screenful of
    folders answered one wake at a time would be a screenful of repaints.
  - **The "Open With…" scan** is a `JobDesk`: the click records what it asked
    about and the chooser opens when the scan lands. The honest "no application
    opens this" refusal is unchanged.
- Tests: the `deferred` suite (`userland/apps/files/src/deferred_tests.rs`) —
  the ask that performs no I/O, no second probe of a folder already in flight or
  in a batch, the whole-batch drain, the answer served once, the empty delivery
  owing no repaint; and `a_pending_probe_leaves_the_entry_unanswered_and_is_asked_again`
  (`lib/browse/src/tests.rs`).

### DESK-12 — The file manager's icon decode off its loop
- **Done.** The last I/O the file manager's loop performed — one bounded
  artwork read plus one parser-sandbox round trip per turn — now runs on the
  DESK-11 reader thread. No interactive loop in the tree performs I/O.
- **Where the split falls, and why there.** `IconPipeline` is the paint side
  alone: the reclaim-governed `ArtworkCache` and the resolver its misses go to.
  The `ArtworkDesk` moved into the reader's `Work` set and is the only thing
  the lock carries, because a picture is handed out as a *borrow* into the
  cache and a borrow cannot outlive a guard — a cache behind that lock could
  lend nothing to a paint. Keeping the whole pipeline under one lock would have
  been worse still: the paint's guard would span the render, and the folder-cue
  probe the render performs takes that same lock.
- **The reader's turn.** `Work` gained `artwork: ArtworkDesk` and `Read` gained
  `Artwork(ArtworkJob)`, served after listings and before the cues. The worker
  takes the job under the lock, drops it, runs the shared
  `tairix_icon::render_artwork` through seams it built once at the top of
  `serve` — including its **own** `ParserSandbox`, so no sandbox handle crosses
  a thread — then retakes the lock to deliver. It nudges the loop when its
  artwork queue drains rather than after each icon, so a folder of fifty
  bundles costs one repaint and the tiles appear together.
- **The paint side.** `DeferredArtwork` is the boxed `ArtworkResolver` over the
  shared desk; with no reader thread `open_icons` boxes `InlineArtwork`
  instead, so the read and the round trip happen in the paint exactly as they
  used to rather than recording jobs nobody will serve. The pressure wake is
  the pair `IconPipeline::trim` (bytes back) plus the desk's `retry_declined`
  (the refused decodes offered again); teardown wipes both the cache's
  retained pictures and the desk's undelivered ones.
- Host tests (`userland/apps/files/src/icons_tests.rs`) drive the real split
  with an `Rc<RefCell<ArtworkDesk>>` standing in for the lock: a paint reads
  and decodes nothing, the reader delivers what the paint recorded, a decode in
  flight is neither re-offered nor re-recorded, a delivery after teardown keeps
  nothing and owes no repaint, a retained decode is never produced twice, every
  tier's refusal settles on the glyph, a declined decode is not re-offered
  until the band moves, and a whole window's grid keeps every tile's artwork
  across repaints and a scroll — with the inline resolver's
  read-and-decode-in-the-paint cost pinned beside them so none of it goes
  vacuous.

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
- **DESK-1 — done.** Process launch is asynchronous: `SPAWN` and the driver
  spawn are **admit-then-load**. The handler does only the synchronous §2.1
  work (cap check, path copy-in + `SPAWN_SELF`, attach/streams,
  `resolve_spawn_credential`, `proc_id`/name/spawn_path, program-vs-bundle
  **syntax** → `NotFound` synchronously), builds a `LoadPlan`
  (`Prebuilt{Cow<'static,[u8]>, requested}` for an embedded/driver image,
  `Bundle{bundle,command}` for a store bundle), and calls
  `KernelSpawnCtx::admit_loading`, returning the minted PID at once — the
  caller never blocks on the read/verify/build.
  - `admit_loading` admits a parked plain kernel kthread and installs the
    manifest-independent admit state (placeholder empty-set caps record via
    the shared `ChildRecordSeed`/`derive_task_record`, streams + wired std
    entries, inherited limits, cwd, device grants + loaded node, parent/child
    wait link), then `unpark`s. The child's loading body — capturing the
    boot-installed `&'static SpawnServices` + `LoadPlan` + seed — runs on the
    child's own task: `body_wait_app_store` (park via `reschedule_current`) →
    obtain rxe+requested (prebuilt directly; bundle via `body_load_bundle`
    under `(uid, effective = credential.ceiling)`) → derive+install the
    effective set (`ceiling ∩ requested`) replacing the placeholder →
    `ArchImageBuilder::build` → register frozen aspace + `stack_span` →
    `become_user` → `enter`. The effective record and frozen space are
    installed strictly before `become_user`, so no child is dispatchable
    under the placeholder authority and no unverified byte is mapped.
  - On any pre-`become_user` failure the child exits with the reserved
    `load_failure_status(errno)` and the refusal is audited
    (`emit_load_refusal`, attributed to the child) + `reclaim_process_bookkeeping`
    (the shared teardown subset a loading child provably holds). The reserved
    `LOAD_*` band + `load_failure_status` / `load_failure_reason` live in
    `lib/abi` (`process.rs`) with round-trip tests; the C view is generated
    (`cargo xtask c-header`, drift-guarded).
  - The old `ProcessSpawn` / `SpawnCtx` / `admit_process` / `NullProcessSpawn`
    / `refuse_spawn` / `SpawnCtxBuild` are deleted; the arch producers only
    `impl ArchImageBuilder` (`alloc_kernel_stack` + `build`). `BootInfo`
    carries `image_builder: &dyn ArchImageBuilder`. The one shared
    build+publish of the boot-installed `SpawnServices` bundle is
    `spawn_services::install_over`, used by `run_phases` and every
    manually-assembled QEMU test kernel alike (`§2.2`). The task-model
    primitive (`Yielder::become_user`, `UserUpgrade`,
    `ThreadControl::pending_upgrade`, the `dispatch_step` install) and the
    `(uid, effective, task)` bundle-read authority split back it.
  - Tests: the `kernel/core` host spawn suite drives the deferred body
    directly — admit returns a PID with no bundle read; effective-set
    derivation (inherit / spawn-as-user / service-account / system-principal);
    missing / unavailable / tampered / malformed / OOM → the matching reserved
    `LOAD_*` status + audit; a valid prebuilt and a valid bundle reach
    `enter`; `resolve_spawn_args` + `emit_load_refusal` units. Per-arch QEMU
    verticals (stack_grow ×3, mem_pin, file_map, sandbox, service_ceiling,
    driver_spawn, driver_unload) exercise the admit-then-load path end to end.
    Docs: `docs/src/architecture/multitasking.md` documents the asynchronous
    launch and the reserved statuses.
- **DESK-2 — done.** The desktop launcher no longer freezes and a refused
  launch is loud, not silent. Each desktop launch (the taskbar's launchers
  and the program-library popup) admits its child and
  returns at once (DESK-1), and the session remembers the child's label and
  spawn path in the `LaunchTable`. The `CHILD_TOKEN` reap runs the shared,
  host-tested `reap_launched` (`userland/gui/session/src/launch.rs`): it
  drains every zombie in one wake (no busy-wait), drops each table
  entry, tears the child's windows down, and — for a child that exited with
  a reserved `LOAD_*` status — writes a terse `stderr` line named by its
  label (`desktop: <App> failed to launch: <reason>`), the reason being
  `lib/abi::load_failure_reason` (the one shared mapping, so every launcher
  words a cause identically). A clean or ordinary exit reports nothing; an
  unrecorded child still reports under a fallback label so no refusal is
  dropped. Responsiveness is a structural consequence of DESK-1 (the
  compositor loop presents once per wake and never blocks on `spawn`), so no
  bespoke display QEMU vertical is needed; the reap-and-report logic is
  covered by `launch::tests`. Docs: the "parent reports the refusal"
  subsection in `docs/src/architecture/multitasking.md`.
- **DESK-3 — done.** The other interactive launch loops now diagnose an
  asynchronous load failure loudly where they once relied on a synchronous
  `spawn` `-errno`. All three report the cause through the one shared
  `lib/abi::load_failure_reason` mapping, so a refusal is worded identically
  everywhere:
  - **Shell (`elsh`).** `Shell::launch_foreground` recognises a reserved
    `LOAD_*` terminal exit on reap and writes `shell: <cmd>: <reason>` to
    stderr, setting `$?` to the coreutils-conventional `127` for a
    missing/unreadable program (`LOAD_NOT_FOUND`) or `126` for every other
    load refusal (`async_load_failure`, `userland/shell/elsh/src/shell.rs`); a
    background job's refusal is stated on stderr as its `[N] Done` line drains
    (`report_finished_jobs`). An ordinary non-zero exit is untouched. Job
    control stays responsive: the shell parks in `wait` while the child loads
    on its own task (DESK-1), so `^C`/`^Z` routing is unaffected.
  - **Terminal (`terminal.app`).** The startup shell is reaped through the one
    `reap_shell` on both the output-stream end-of-stream and child-exit arms
    (the wait-set may wake on either first), and a reserved `LOAD_*` status is
    turned into a fail-loud `terminal: shell failed to launch: <reason>` on
    stderr with a reserved exit code — restoring the diagnosis the old
    synchronous `spawn_attached < 0` gave. The classification is the
    host-tested `tairix_terminal::shell_load_failure`.
  - **Login (`login`).** A session that `spawn` admits but that then fails its
    own image load exits with a reserved `LOAD_*` status; `start_session`
    records it as a `SESSION_LAUNCH_FAILED` audit event *with its reason*
    (not a normal `SESSION_ENDED`) and degrades gracefully by returning to the
    login prompt — strictly better than the old terminate-on-synchronous-errno.
  - Tests: elsh (`shell::tests`) covers each reserved status → named stderr +
    `126`/`127`, an ordinary non-zero exit left undiagnosed, and the background
    reap report; the terminal (`shell_load_failure_classifies_reserved_statuses`)
    and login (`session_that_fails_its_async_load_is_reported_as_a_launch_failure`)
    cover their classifications.
- **DESK-4 — done** (§4 above).
- **DESK-8 — done.** Icon artwork is decoded on a worker thread; no icon
  resolution happens inside a paint.
- **DESK-9 — done.** The program catalogue and the associations its bundles
  declare are one snapshot read on a worker; the launcher opens on the
  catalogue in hand and adopts the fresh one when it lands.
- **DESK-10 — done.** No control's value is tied to a store write: a
  continuous control reports where it settled, the terminal separates the
  profile it renders from the one the store holds, and the desktop session's
  persist-then-adopt happens on a worker.
- **DESK-11 — done.** Every unbounded read the file manager makes — listings,
  folder cues, and the "Open With…" bundle scan — runs on one reader thread.
- **DESK-12 — done.** The file manager's icon decode runs on the DESK-11
  reader thread; no interactive loop in the tree performs I/O.
- **DESK-15 — done.** No interactive or serve loop waits on a statistics
  report; `tairix_rt::submit::Submission` is the one shape both publishers hand
  them over with.
- **DESK-16 — done.** The settings sheet's picture is retained and its paint
  scoped to what its controls reported.
- **DESK-17 — done.** No desk re-runs an answered job; the listing and
  wallpaper workers park instead of looping.
- **DESK-18 — done.** The font protocol is per run: a cold run of text costs
  one `FONT_ENDPOINT` round trip rather than one per character, and a warm one
  costs none.
- **DESK-13 — done.** A profile change costs only the work the field that moved
  implies; the terminal is the last surface off the shared damage discipline
  and was the only one with this coupling.
- **DESK-14 — done.** The wallpaper chooser's *Apply* runs on a worker; the
  desk-plus-worker arrangement is one shared `lib/rt::work`; and the wait a
  worker's answer arrives on can end without an event, which also closed a
  busy-spin in the file manager.
- **DESK-5 … DESK-7 — planned.**

---

## 6. Cross-references

- `plans/FIX-SYSCALL.md` — P-5b (syscalls run with IRQs enabled; the
  kernel stays non-preemptible per task) is *why* a long syscall body on
  the desktop's task freezes the desktop even though the machine is not
  wedged. This plan removes the long body from the caller's task.
- `plans/SPAWN.md` — the `SPAWN` syscall, admit path, and parent/child
  wait link this plan defers the load behind.
- `plans/FONT-SERVICE.md` — the *payload* half of the slow launch: every GUI
  `Run` image embeds ~10 MB of font data (atlas + TrueType faces), so the
  launch path reads/verifies/copies ~10 MB per start. That plan moves the
  font data into a single sandboxed OS service so no app carries it;
  complementary to this plan's async launch and demand-paged/CoW image build.
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
- `AGENTS.md` `§28` — the rule this plan enforces, stated outright; plus
  `§2.16`, `§2.23`, `§5.4`, `§17.2`, `§19`, `§24`, `§24.1`, `§26.1`, `§26.2`,
  `§26.3`, `§26.6`, `§26.7` — the rules this plan enforces (including the demand-paged, CoW-shared, verified image of
  §2.6, which puts program loading strictly ahead of Linux).
