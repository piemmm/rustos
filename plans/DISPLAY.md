# DISPLAY.md — Seat ownership: the display/console locking model

This is a staged build plan for RustOS's **seat** subsystem: the exclusive,
owner-tracked, revocable ownership of a physical display + keyboard + pointer,
and the controlling-terminal / foreground arbitration for the text consoles
that share one. It is **binding under `AGENTS.md`** — read `AGENTS.md`,
`PLAN.md`, and `plans/PI.md` (P11, "input follows the surface owner") first;
every rule in all three applies here without exception.

It exists because the console/graphics *ownership and locking* story is not
yet at parity with Linux (DRM master + `logind` seats + tty controlling
terminal), and because the charter's fail-closed, capability-first, no-ambient
model lets us do it **better** than Linux rather than merely copy it. The
foundations already in the tree — capability-gated framebuffer/input, the
kernel-arbitrated `InputFocus`, fail-closed streams — are sound; this plan
adds the missing *arbitration* layer on top of them, evolving the existing
seams in place (`AGENTS.md` §2.13), never bolting a second model beside them.

## 0. Scope and decisions (binding for this plan)

- **The seat is a first-class kernel object with a tracked, exclusive owner.**
  Today `display_acquire` / `display_release` are a global `AtomicBool` in
  `kernel/core/src/input_focus.rs` with **no owner identity**: any holder of
  `CAP_DISPLAY` can flip the foreground regardless of who currently holds it
  (last-writer-wins), and a `release` is not checked against being the
  acquirer. The `CAP_DISPLAY` rustdoc claims "an ordinary task cannot …
  steal keyboard focus from the active session," a guarantee the `AtomicBool`
  does not actually enforce — it rests solely on the coarse fact that exactly
  one principal is granted the capability. That mismatch between the stated
  invariant and the enforced behaviour is a defect under the review gate
  (`AGENTS.md` §5.4 fail-closed / capability-before-state, §23.1 no
  over-claiming docs) and is **fixed in this plan, not deferred** (§2.17,
  §2.18): the owner becomes real and the doc becomes true.

- **We evolve `CAP_DISPLAY` in place; we do not add a `v2` syscall pair.**
  RustOS has not shipped, so `abi-v1` is still mutable (`AGENTS.md` §9,
  §2.13). `display_acquire` / `display_release` (`abi-v1` numbers 23 / 24)
  gain owner semantics in place, and every caller is updated in the same
  change. No shim, no `display_acquire2`, no compatibility flag.

- **Exactly one new capability, and only where a coarser one will not do.**
  Per capability-minimalism (`AGENTS.md` §5.2), the *ownership* of a seat
  stays gated by the existing `CAP_DISPLAY` (owning the surface is exactly
  what it already means). Only the **seat-multiplexing authority** — the
  `chvt`/`logind`-equivalent power to switch which session is foreground
  across *all* seats and to *forcibly revoke* a wedged owner — is a genuinely
  new security boundary over a *group* of resources (every seat, every
  session's focus), so it becomes `CAP_SEAT_ADMIN`, introduced **in the same
  stage as the seat-manager service that holds it and the switch/revoke
  entry points that enforce it** (§5.2 rule 2 — no capability ahead of its
  holder). No other new capability is added; framebuffer mapping stays
  `CAP_MMIO_MAP`, input read stays `CAP_INPUT_READ`, input injection stays
  `CAP_INPUT_INJECT`.

- **Fail closed, always** (`AGENTS.md` §5.4, §2.9): a non-owner's
  acquire/release/present is denied (a typed `Errno`, never a silent flip);
  an unowned seat routes input to its text foreground, never to a stale
  desktop channel; a revoked owner's subsequent present/read denies rather
  than reaching the framebuffer. No panic on any of these paths.

- **No target coupling** (`AGENTS.md` §2.20): the seat object, its owner
  table, the lease/revoke logic, and the foreground arbitration are
  arch-neutral `kernel/*` + `lib/*`. A concrete display's framebuffer still
  arrives by discovery (`plans/PI.md`); the seat layer never names a board.

- **Headless stays first-class** (`AGENTS.md` §17.3): a build with no display
  node has a text-only seat (a console foreground with no desktop owner ever
  possible), and no `userland/gui/*` edge is introduced into the seat layer.

- **No stubs** (`AGENTS.md` §15.1): each stage ships code **plus** tests
  **plus** docs, and is "done" only when the whole-project gate (§7) is green.

## 1. The gap this plan closes (against Linux)

| Concern | Linux | RustOS today | This plan |
| --- | --- | --- | --- |
| Exclusive display owner | DRM master per-`fd`, revocable | global `AtomicBool`, no owner | kernel-tracked owner task id + revocable lease |
| Steal-focus protection | master check in ioctl | *claimed* in docs, not enforced | owner-checked acquire/release/present, enforced |
| Present ↔ ownership coupling | scanout gated by master | `CAP_MMIO_MAP` decoupled from `CAP_DISPLAY` | present right derived from the live seat lease |
| Seat multiplexing (VT switch) | `chvt`/`VT_ACTIVATE`, `logind` seats | single text-vs-desktop boolean | `CAP_SEAT_ADMIN` foreground switch across sessions |
| Controlling terminal / foreground | session leader + fg pgroup + `SIGTTIN/TTOU` | inherited fd table + `CAP_CONSOLE_READ` gate | per-console controlling-owner + fg handoff, capability-gated |
| Multi-head / multi-seat | DRM connectors + `logind` seats | `consoles[0]` == "the display" | N independent seat objects, one owner each |

## 2. Design — "better than Linux"

RustOS improves on Linux on the axes the charter already privileges:

1. **The owner is an unforgeable kernel fact, not a coarse capability grant.**
   Linux's master is a per-`fd` flag; RustOS records the *owning task id* on
   the seat and checks it on every ownership-changing call, so the "cannot
   steal focus" guarantee holds even if two principals legitimately hold
   `CAP_DISPLAY` (two graphical sessions). No ambient authority (§4).

2. **A lease is revocable and revocation is observable.** Instead of Linux's
   coarse DROP_MASTER, the seat lease is an object the owner holds; a
   `CAP_SEAT_ADMIN` holder (the seat manager) can **revoke** it (e.g. to
   switch sessions or to reclaim a wedged owner). After revocation the old
   owner's present/keyboard-read **fail closed** with a distinct typed error,
   so a well-behaved compositor learns it lost the seat rather than scribbling
   over the new foreground. The revoke is a security-relevant decision logged
   with a stable event id (`AGENTS.md` §19.4).

3. **The present right is *derived from* the live lease, not a separate,
   forgeable capability.** Mapping the framebuffer (`CAP_MMIO_MAP` to the
   display driver) and *owning the seat* (`CAP_DISPLAY`) are decoupled today,
   so "I can write pixels" and "I own the screen" are two different facts.
   This plan makes scanout/flip the driver performs on behalf of a client
   gate on the client's *current* seat lease: a client whose lease was revoked
   cannot present even though its framebuffer mapping still exists. Linux
   relies on the compositor cooperating; RustOS enforces it.

4. **Controlling-terminal arbitration without signal races.** Linux uses
   `SIGTTIN`/`SIGTTOU` and a foreground process group, historically racy and
   spoofable. RustOS makes the *controlling owner* of a text console a
   kernel-tracked task id: only the console's foreground owner drains its
   input queue; a background reader is denied (fail closed) rather than
   stopped by an async signal. Foreground handoff is an explicit,
   capability-checked call, not an ambient side effect of `tcsetpgrp`.

5. **Seats are independent objects.** A machine with two displays is two
   seats, each with its own owner, lease, foreground console set, and input
   routing — the DRM-connector + `logind`-seat idea, but as one uniform
   kernel object rather than two subsystems glued in userspace. Hotplug
   adds/removes a seat through the same hardware-tree discovery path
   (`plans/PI.md`, `AGENTS.md` §18.4); no reboot.

6. **Secret hygiene is already ahead** and is preserved: the desktop keyboard
   channel zeroes drained records in place; the seat layer keeps that and
   extends zeroing to any per-seat input buffering it adds (`AGENTS.md` §4).

## 3. Layering (one-way edges, `AGENTS.md` §17.4)

```
lib/abi        → seat ABI types: SeatId, SeatState, lease/revoke error codes,
                 the seat sysinfo query record. ABI-disciplined (§9): versioned,
                 hashed, frozen on the first release.
lib/seat       → lib/abi, lib/* only. The arch-neutral seat model: the owner
                 table, lease grant/revoke state machine, per-seat foreground
                 console set, and the input-routing decision. no_std, host-tested.
kernel/core    → lib/seat, kernel/* : hosts one seat registry per running
                 kernel, folds the existing InputFocus arbiter into a per-seat
                 sink, and wires the display_* / seat_* syscalls.
drivers/display/* → present gated on the caller's live seat lease (via lib/abi
                 seat handle passed through DriverHost); never names a board.
userland/system/seatmgr (new) → holds CAP_SEAT_ADMIN; switches foreground and
                 revokes leases. A system service, not GUI (headless-safe).
userland/gui/wm → holds CAP_DISPLAY; acquires/renders/releases its seat only.
```

`lib/seat` is arch-neutral and shared, so the owner/lease logic has exactly
one definition (`AGENTS.md` §2.2); the in-kernel registry and the userspace
seat manager both build on it, never re-deriving the state machine.

## 4. Capabilities and ABI

- **`CAP_DISPLAY` (existing, id 23) — evolved in place.** Still the gate on
  `display_acquire` / `display_release`, but those now bind the acquiring
  task id as the seat owner and check ownership on release/present. Its
  rustdoc is rewritten so the enforced behaviour matches the claim (the doc
  no longer over-promises; it states the owner-checked, revocable lease).

- **`CAP_SEAT_ADMIN` (new) — introduced with its enforcement point (D3).**
  Guards the *group* authority to switch the foreground session across seats
  and to forcibly revoke a lease — a real security boundary over every seat,
  not a single object (§5.2 rule 1). Its sole holder is the seat-manager
  service, added in the same stage (§5.2 rule 2). No existing capability
  expresses "administer all seats" at this granularity (§5.2 rule 3):
  `CAP_DISPLAY` owns *one* surface, `CAP_INPUT_*` route input, none can
  revoke another principal's lease.

- **New syscalls (ABI-disciplined, `AGENTS.md` §9 — versioned, hashed,
  generated into the C view, frozen on release):**
  - `seat_switch(seat_id, target_session)` — gated `CAP_SEAT_ADMIN`;
    fail-closed foreground handoff, logged.
  - `seat_revoke(seat_id)` — gated `CAP_SEAT_ADMIN`; revoke the current
    lease, logged; the old owner's next present/read denies.
  - `seat_query` folded into the System Information API (`AGENTS.md` §16.6)
    behind a privileged query (`CAP_SYSINFO_HW`-class): enumerate seats,
    owners, foreground console — never a `/proc`-style file (§16.1).
  - `display_acquire`/`display_release` keep their numbers (23/24), gain
    owner semantics, and return typed errors (`NotOwner`, `SeatBusy`,
    `SeatRevoked`) instead of always `Ok`.

  The syscall table stays generated from `lib/abi/src/syscalls.rs`
  (`AGENTS.md` §9); `cargo xtask abi-check` / `c-header` must stay green.

## 5. Work breakdown (stages)

Each stage is a reviewable chunk that ships code + tests + docs and is not
started before its predecessor is green on the whole-project gate (§7). At the
end of each stage, write the continuation prompt for the next to
`.junie/next-display-prompt.md` (overwrite each time), recording what landed
and the exact next work, in the style of the other plans' continuation
prompts. Plan files state current state, not history (`AGENTS.md` §13).

### Stage D1 — `lib/seat`: the arch-neutral seat model `[ ]`

**Deliverables**
- New `no_std` crate `lib/seat` (update `AGENTS.md` §3 and `PLAN.md`, §6):
  - `SeatId`, `SeatState { owner: Option<TaskId>, foreground: ForegroundSink,
    lease: LeaseState }`.
  - The lease state machine: `acquire(task)` (fail if owned by another),
    `release(task)` (fail if not the owner), `revoke()` (admin), with typed
    outcomes for every illegal transition (illegal states unrepresentable,
    §23.2).
  - The input-routing decision folded in from `InputFocus`: given a seat's
    state, where does a key edge go (text foreground console vs. the owner's
    desktop channel)?
- Rustdoc on every public item; `README.md` stability tier `experimental`
  (§6). `docs/src/desktop/seat.md` page (new) describing the model.

**Done when:** `lib/seat` host-tests cover acquire/release/revoke, non-owner
denial, unowned→text routing, and owned→desktop routing; the whole-project
gate is green.

### Stage D2 — fold `InputFocus` into a per-seat sink; owner-checked `display_*` `[ ]`

**Deliverables**
- `kernel/core` hosts a seat registry (one seat per discovered display node;
  a text-only seat when none). The existing `InputFocus` becomes the seat's
  input-routing arm driven by `lib/seat` state — no second routing definition
  (§2.2); the `AtomicBool` foreground is replaced by the seat's owner/lease.
- `display_acquire` binds `caller.task_id` as the owner (fail `SeatBusy` if
  already owned by another); `display_release` checks ownership (fail
  `NotOwner`); both stop ignoring `_caller`.
- `CAP_DISPLAY` rustdoc rewritten to state the enforced, owner-checked,
  revocable behaviour (remove the over-claim).

**Done when:** kernel tests prove a non-owner cannot release/steal a held
seat, a released seat returns input to the text foreground, and the docs match
the enforcement; gate green. Any defect surfaced here is fixed in this stage
(§2.18).

### Stage D3 — `CAP_SEAT_ADMIN`, `seat_switch` / `seat_revoke`, and `seatmgr` `[ ]`

**Deliverables**
- Add `CAP_SEAT_ADMIN` **with** the `seat_switch` / `seat_revoke` syscalls
  that enforce it and the new `userland/system/seatmgr` service that holds it
  (§5.2 rule 2). Revocation makes the old owner's subsequent present/read
  fail closed with `SeatRevoked`; every switch/revoke is audit-logged (§19.4).
- `seat_query` via the System Information API (§16.6); the `sysinfo` tool
  gains a seats view (no `/proc`, §16.1).

**Done when:** tests prove admin-gated switch/revoke, fail-closed old-owner
access post-revoke, audit events emitted, and unprivileged callers denied;
gate green.

### Stage D4 — present right derived from the live lease `[ ]`

**Deliverables**
- The display driver's present/flip path (`drivers/display/*`) gates on the
  caller's *current* seat lease (threaded through `DriverHost` as a `lib/abi`
  seat handle), so a revoked client cannot scanout though its framebuffer
  mapping persists. Arch-neutral; no board names (§2.20).

**Done when:** a QEMU display vertical proves a revoked client's present is
refused while the new foreground renders; gate green.

### Stage D5 — per-console controlling owner + foreground handoff `[ ]`

**Deliverables**
- The text-console path (`stream_read` in `kernel/core/src/syscalls.rs`)
  gains a kernel-tracked controlling owner per console: only the foreground
  owner drains the input queue; a background reader is denied fail-closed
  (no async signal race). Foreground handoff is an explicit capability-checked
  call, inherited/intersected across spawn like other rights (§5.2).

**Done when:** tests prove two tasks on one console cannot both drain input,
handoff transfers the drain right, and background reads fail closed; gate
green.

### Stage D6 — multi-seat / hotplug `[ ]`

**Deliverables**
- Multiple display nodes yield multiple independent seats; hotplug add/remove
  drives seat create/destroy through the existing discovery path (§18.4),
  reusing D1 state — no per-board logic.

**Done when:** a multi-display QEMU vertical (where emulable) proves two
seats with independent owners and input routing; a hotplug test proves
create/destroy with no reboot; gate green.

## 6. Tests, docs, and gate (binding)

- Every stage: unit tests in-crate, integration/QEMU verticals where hardware
  is emulable, rustdoc on all public items, the `docs/src/desktop/seat.md`
  page kept current in the same change (`AGENTS.md` §2.8, §13).
- Coverage: `lib/seat`, and the `kernel/core` seat/ownership paths, meet the
  kernel targets (§7). `kernel/sec`/`kernel/ipc`-class ≥ 95% where the new
  capability check lives.
- Fuzz the new ABI decoders and the `seat_*` syscalls (`AGENTS.md` §19.6).
- Definition of done per stage is the whole-project gate (§7): `cargo fmt
  --all`, `cargo xtask ci` (once), `cargo xtask fuzz --secs 5`, and the
  `tools/ci/soak.sh both --secs 20` developer soak — all green, output quoted.

## 7. What this plan explicitly does *not* do

- It does **not** add a `/dev`, `/sys`, or a VT device file — seats are
  objects reached through the capability-gated ABI + System Information API,
  never a virtual filesystem (`AGENTS.md` §16.1).
- It does **not** move display policy into the kernel beyond ownership
  arbitration: rendering, theming, and window management stay in
  `userland/gui/*` (microkernel-leaning, §4; optional desktop, §17.3).
- It does **not** introduce any capability beyond the single `CAP_SEAT_ADMIN`,
  and that only alongside its holder and enforcement point (§5.2).
