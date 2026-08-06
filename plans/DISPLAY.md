# DISPLAY.md — Seat ownership: the display/console locking model

This is a staged build plan for TAIRiX's **seat** subsystem: the exclusive,
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
kernel-arbitrated seat registry, fail-closed streams — are sound; this plan
adds the missing *arbitration* layers on top of them, evolving the existing
seams in place (`AGENTS.md` §2.13), never bolting a second model beside them.

## 0. Scope and decisions (binding for this plan)

- **The seat is a first-class kernel object with a tracked, exclusive owner.**
  Since D2 this is enforced: `display_acquire` / `display_release` bind and
  check the kernel-attested owning task on the kernel seat registry
  (`kernel/core/src/seat.rs`, which replaced the owner-less `AtomicBool`
  `InputFocus` arbiter), so a held seat is never displaced
  (`Errno::SeatBusy`), a release is owner-checked (`Errno::SeatNotOwner`),
  and the `CAP_DISPLAY` rustdoc states exactly the enforced, owner-checked,
  revocable behaviour — the doc is true, not an over-claim (`AGENTS.md`
  §5.4, §23.1).

- **We evolve `CAP_DISPLAY` in place; we do not add a `v2` syscall pair.**
  TAIRiX has not shipped, so `abi-v1` is still mutable (`AGENTS.md` §9,
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

| Concern | Linux | TAIRiX today | This plan |
| --- | --- | --- | --- |
| Exclusive display owner | DRM master per-`fd`, revocable | kernel-tracked owner task id (D2) | + revocable lease enforcement end-to-end |
| Steal-focus protection | master check in ioctl | owner-checked acquire/release/read (D2) | + owner-checked present |
| Present ↔ ownership coupling | scanout gated by master | `CAP_MMIO_MAP` decoupled from `CAP_DISPLAY` | present right derived from the live seat lease |
| Seat multiplexing (VT switch) | `chvt`/`VT_ACTIVATE`, `logind` seats | single text-vs-desktop boolean | `CAP_SEAT_ADMIN` foreground switch across sessions |
| Controlling terminal / foreground | session leader + fg pgroup + `SIGTTIN/TTOU` | inherited fd table + `CAP_CONSOLE_READ` gate | per-console controlling-owner + fg handoff, capability-gated |
| Multi-head / multi-seat | DRM connectors + `logind` seats | `consoles[0]` == "the display" | N independent seat objects, one owner each |

## 2. Design — "better than Linux"

TAIRiX improves on Linux on the axes the charter already privileges:

1. **The owner is an unforgeable kernel fact, not a coarse capability grant.**
   Linux's master is a per-`fd` flag; TAIRiX records the *owning task id* on
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
   relies on the compositor cooperating; TAIRiX enforces it.

4. **Controlling-terminal arbitration without signal races.** Linux uses
   `SIGTTIN`/`SIGTTOU` and a foreground process group, historically racy and
   spoofable. TAIRiX makes the *controlling owner* of a text console a
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
   The same rule already binds the seat's *pixels*: the desktop's rasterised
   caches (cursors, notification glyphs, window furniture) are charged to
   the owning seat and wiped on release, and a window's content surface is
   overwritten before it is dropped — whether it is released to reclaim
   memory or torn down on logout or seat loss. A revoked or ended seat
   leaves no readable frame behind.

## 3. Layering (one-way edges, `AGENTS.md` §17.4)

```
lib/abi        → seat ABI types: SeatId, SeatState, lease/revoke error codes,
                 the seat sysinfo query record. ABI-disciplined (§9): versioned,
                 hashed, frozen on the first release.
lib/seat       → lib/abi, lib/* only. The arch-neutral seat model: the owner
                 table, lease grant/revoke state machine, per-seat foreground
                 console set, and the input-routing decision. no_std, host-tested.
kernel/core    → lib/seat, kernel/* : hosts one seat registry per running
                 kernel (src/seat.rs — the folded-in per-seat input-routing
                 sink) and wires the display_* / seat_* syscalls.
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

- **The seat's desktop record is deliberately ungated.**
  `tairix_abi::desktop::DesktopInfo` — the screen extent, the UI scale, and
  the active appearance — reaches an application over the window channel it
  already holds: `WindowRequest::QueryDesktop` answers it, and
  `WindowEvent::DesktopChanged` re-sends it to every live window when the
  session changes any of it. **Do not add a capability to it.** It describes
  the caller's own seat, names no other principal's data, and authorises
  nothing; gating it would only force every application back to guessing at
  facts the user can see by looking at their monitor, which is precisely the
  defect it exists to remove. The display service's own `Query` stays
  lease-gated: that one is a step toward *driving* the output, not describing
  it.

## 5. Work breakdown (stages)

Each stage is a reviewable chunk that ships code + tests + docs and is not
started before its predecessor is green on the whole-project gate (§7). At the
end of each stage, write the continuation prompt for the next to
`.junie/next-display-prompt.md` (overwrite each time), recording what landed
and the exact next work, in the style of the other plans' continuation
prompts. Plan files state current state, not history (`AGENTS.md` §13).

### Open — a user-settable UI scale

The density is now honest end to end: the compositor owns the output's
`Scale`, the session reports it in the desktop record, and every application
and every session-drawn surface resolves its lengths through the reported
value rather than a hard-coded 100%. What is still missing is any way for a
user to *choose* it — nothing in the tree calls `Compositor::set_scale` or
`DesktopShell::set_scale` outside tests, so the desktop always runs at the
reference density.

What remains: a persisted per-seat scale in the desktop's own settings, a
control that changes it (the Switchboard capsule's quick actions are the
natural home, beside the light/dark pair), and the runtime application —
`DesktopShell::set_scale` followed by the existing `announce_desktop`, which
already relays a change to every open window. No ABI change is needed: the
wire already carries the percentage and every consumer already reads it.

### Stage D1 — `lib/seat`: the arch-neutral seat model `[x]`

**Done.** The dependency-free `no_std` crate `lib/seat` (`tairix-seat`,
registered in `AGENTS.md` §3 and the workspace) is the one seat state
machine:

- `SeatId` / `SeatOwner` (the kernel-attested task identity as an opaque
  newtype — `kernel/core` converts its `TaskId` at the boundary, keeping the
  §17.4 layering) / `ConsoleIndex` / `Lease { owner, generation }` /
  `SeatError { SeatBusy, AlreadyOwner, NotOwner, SeatUnowned, SeatRevoked }`.
- `SeatState { lease, foreground console }` with the total, fail-closed
  transitions: `acquire` (refuses `SeatBusy`/`AlreadyOwner`, mints a lease
  with a per-seat monotonic generation), `release` (owner-checked,
  `NotOwner` otherwise), `revoke` (admin path; returns the evicted owner for
  the audit log), and the owner-gated `access` check the present/keyboard
  paths will apply (D2/D4).
- Revocation is observable and acknowledgeable: the evicted owner's next
  `access`/`release` sees the distinct `SeatRevoked`; any fresh `acquire`
  (including the evicted task's explicit reacquire) clears the marker.
- `route()` is the folded-in input-routing decision: `Desktop(owner)` while
  held, `Text(foreground console)` while unowned — including immediately
  after a revoke, never a stale desktop channel.

Rustdoc on every public item; README tier `experimental`;
`docs/src/desktop/seat.md` describes the model. 17 host tests cover
acquire/release/revoke, non-owner and revoked-owner denial, both routing
directions, generation monotonicity, and foreground retargeting.

### Stage D2 — fold `InputFocus` into a per-seat sink; owner-checked `display_*` `[x]`

**Done.** The kernel seat registry (`kernel/core/src/seat.rs`,
`SeatRegistry`) replaced the owner-less `InputFocus` arbiter: it hosts
`tairix_seat::SeatState` under its own lock next to the text sink and the
bounded, zeroing desktop keyboard channel — one routing definition, driven
by `SeatState::route` (§2.2). `display_acquire` binds `caller.task_id` as
the owner and `display_release` is owner-checked; the typed refusals are
the `abi-v1` errnos `SeatBusy` (24), `SeatNotOwner` (25), `SeatRevoked`
(26) (a double acquire surfaces `AlreadyExists`), mapped from `SeatError`
in exactly one place (`seat_errno`) and generated into the C headers. The
desktop `keyboard_read` drain is owner-gated through `SeatState::access`,
so a non-owner `CAP_INPUT_READ` holder cannot siphon the owner's
keystrokes. The `CAP_DISPLAY` / `CAP_INPUT_READ` rustdoc, the syscall-table
and wrapper docs (`lib/rt`, `lib/abi-sys`), and
`docs/src/desktop/seat.md` state the enforced behaviour. Kernel host tests
prove a non-owner cannot steal/release/drain a held seat and a released
seat returns input to the text foreground.

### Stage D3 — `CAP_SEAT_ADMIN`, `seat_switch` / `seat_revoke`, and `seatmgr` `[x]`

**Done.** The single new capability `CAP_SEAT_ADMIN` (id 33) landed with
its two enforcement points and its sole holder in one change (§5.2 rule 2):

- `seat_switch` (`abi-v1` 70) retargets an unowned seat's foreground text
  console; the seat id and console index are
  validated against the live topology before any state changes (`NotFound`
  otherwise). `seat_revoke` (`abi-v1` 71) drives `SeatState::revoke` on
  the kernel registry: the seat becomes acquirable, input returns to the
  text foreground, and the evicted owner's next owner-gated call fails
  closed with `SeatRevoked`. Both are `CAP_SEAT_ADMIN`-gated at dispatch
  and audit-logged (`SEAT_SWITCHED` 4051; `SEAT_LEASE_REVOKED` 4052, with
  the evicted task id). Wrappers in `lib/rt`; `tairix_sys_seat_*` stubs in
  `lib/abi-sys`; C headers regenerated.
- `userland/system/seatmgr` (installed at
  `/System/Services/seatmgr.app/Run`, launched by PID 1, headless-safe) is
  the sole manifest holder. It binds the reserved `SEATMGR_ENDPOINT`
  (`tairix_abi::seat`, squat-protected) and serves the fixed-width
  `SeatAdminRequest` (`Switch`/`Revoke`), requiring each requester's
  attested origin to itself carry `CAP_SEAT_ADMIN` before forwarding — the
  kernel re-checks the capability and every index on each syscall. Own
  audit range `14000..15000`; decoders covered by the `lib/abi` fuzz
  harness.
- Seat observability is the System Information API (§16.6, never `/proc`):
  `IntrospectDomain::Seats` (served straight from the kernel seat
  registry), the `SEAT_LIST` query (id 12, `CAP_SYSINFO_HW`, audited)
  returning `SeatRecord`s, and the `sysinfo seats` view (Help updated in
  all locales).

Tests prove admin-gated switch/revoke with fail-closed topology
validation, unprivileged-requester denial before any state, fail-closed
old-owner access post-revoke, the audit events (including the evicted
identity), the introspection paging contract, and the manifest/AppInfo
pins. `docs/src/desktop/seat.md`, `docs/src/userland/seatmgr.md`, and the
kernel/sysinfod pages state the enforced behaviour.

### Stage D4 — present right derived from the live lease `[x]`

**Done.** The present right is derived from the live seat lease, not from
the framebuffer mapping:

- `display_acquire` returns the minted lease's generation (`>= 1`), so the
  client holds the `lib/abi` handle (`tairix_abi::seat::SeatLease`:
  seat id + owner task + generation; `SEAT_PRIMARY` names the boot seat).
- The check has one definition, `tairix_seat::SeatState::verify` (exact
  live owner-and-generation; the evicted owner's handle sees the distinct
  `SeatRevoked`, every other dead handle `NotOwner`), hosted kernel-side
  as `SeatRegistry::present_gate` — a `PresentGate` bound to one client's
  lease that re-reads the live lease under the registry lock on every
  call.
- The gate reaches a driver as the host seam
  `DriverHost::seat_gate() -> Option<&dyn SeatGate>` (default `None`; a
  seatless headless/bring-up host presents ungated, §17.3). All three
  display drivers (`framebuffer`, `vesa`, `rpi_hvs` — software `present`
  and hardware `present_layers` alike) consult it *first*, before any
  validation or surface access; a revoked client is refused with the new
  `DriverError::SeatRevoked` (14, → `Errno::SeatRevoked`) while its
  mapping persists. Arch-neutral; no board names in the gate.
- Proven by driver unit tests (refused present leaves the surface
  untouched, both hvs paths gated) and the aarch64 framebuffer QEMU
  vertical's seat phase on a real `SeatRegistry`: owner presents → revoke
  → evicted present refused with the surface intact → new foreground's
  fresh lease renders (generation monotonicity asserted).

Docs: `docs/src/desktop/seat.md`, `docs/src/drivers/display.md`, driver
READMEs, syscall table row 23 (`u64` lease generation).

### Stage D5 — per-console controlling owner + foreground handoff `[x]`

**Done.** Each text console carries a kernel-tracked controlling
(foreground) owner, enforced fail-closed with no `SIGTTIN`-style signal
race:

- `ConsoleDevice` (`kernel/core/src/console.rs`) records
  `{owner, granter}` (lock-free atomics the ISR input filter reads,
  compound transitions serialised under the device's `fg` lock) with the
  checked transitions `grant_foreground` (honoured only from an unowned
  console, the recorded granter, or the current owner delegating to its
  own child), `release_foreground` (granter/owner only; unowned release
  is an idempotent success), and `clear_dead_foreground` (compare-and-
  clear). The unchecked setter is gone.
- `stream_read` and `stream_input_mode` share one gate
  (`check_console_foreground`): while an owner is recorded, any other
  task is refused with the typed `Errno::NotForeground` (new `abi-v1`
  errno 27, generated into the C headers) before any input is consumed
  or the discipline changes; an unowned console reads openly. No new
  capability (§5.2): the authority is the inherited console descriptor,
  the parent/child relation (`ProcessWait::authorise_child`), and the
  owner-checked slot transition — the drain right only moves down the
  spawn chain, inherited and intersected.
- A vanished owner never wedges the console: the `exit` handler releases
  the exiting task's ownership, and the gate clears an owner the process
  bookkeeping proves dead (`ProcessWait::is_live`; the inert default
  reports live, so an unproven death keeps denying — heal, never widen).
- `console_foreground` (72) keeps its number and gains the grant/release
  semantics in place; `^C`/`^Z` foreground signal delivery (SP9) rides
  the same slot, so signal target and drain right can never diverge.
  elsh's mark-around-wait wiring is unchanged.

Kernel host tests prove two tasks on one console cannot both drain (the
refused reader consumes nothing), handoff transfers the drain right,
background reads and mode changes fail closed, bystander grant/clear
steals are refused, and both no-wedge paths (exit release, gate healing);
device-level transition tests and `ProcessTable::is_live` tests back
them. Docs: `docs/src/desktop/seat.md` (D5 section),
`docs/src/architecture/syscalls.md` (rows 13/21/72), the `lib/abi` /
`lib/rt` / `lib/abi-sys` rustdoc.

### Stage D6 — multi-seat / hotplug `[x]`

**Done.** The kernel seat registry hosts every seat on the machine, each
an independent `tairix_seat::SeatState` with its own lock, text sink, and
zeroing keyboard channel:

- The boot seat (`SEAT_PRIMARY`, id 0) always exists — text-only on a
  headless build. Every further seat is minted by discovery: a
  display-class node published through `hw_emit_node` creates one
  (`SeatRegistry::attach_display`) and its removal through
  `hw_remove_node` — the seam now reports every removed subtree node id —
  destroys it (`detach_display`), audited as `SEAT_CREATED` (4053) /
  `SEAT_DESTROYED` (4054) with the seat and node ids. No reboot, no
  parallel device list, no board coupling.
- The seat-addressed syscalls were generalised in place (§2.13):
  `display_acquire`/`display_release` take the seat id, and
  `key_inject`/`keyboard_read` name the seat first
  (`stream_read`-style), so a keyboard driver injects for the seat its
  device belongs to (the boot seat for a directly attached keyboard) and
  each seat's channel is drained only by its own owner. An unknown or
  destroyed seat fails closed `NotFound` on every path, including the
  present gate (which re-resolves the seat per call, so a hot-removed
  display kills a still-held lease's authority instantly). Seat ids are
  monotonic and never reused. `SEAT_LIST`/`IntrospectDomain::Seats` pages
  every seat by whole record, boot seat first.
- Proven by the aarch64 framebuffer QEMU vertical's multi-seat phase (two
  seats with independent owners and input routing, present under the
  hotplug seat's lease, detach → dead-seat present refused with the boot
  seat intact) and kernel host tests driving create/destroy through the
  real `hw_emit_node`/`hw_remove_node` handlers.

Docs: `docs/src/desktop/seat.md` (D6 section),
`docs/src/architecture/syscalls.md` (rows 22–25), the `lib/abi` /
`lib/rt` / `lib/abi-sys` rustdoc, and the regenerated C headers.

What D6 deliberately does **not** do: assigning *which input device*
feeds *which seat* beyond the boot seat's directly attached keyboard is
seat-topology policy for the seat manager, staged with the desktop
session work (`plans/PI.md` P11 / `PLAN.md` CU6), not a kernel-side
default.

### Stage D7 — the display-client present path (the graphical session goes live)

**Status: done — D7a–D7d complete.** D1–D6 made the seat an
enforced, revocable kernel object and derived the present right from the
live lease — but the only presenters so far are kernel-side fixtures. D7 is the
missing transport: a user-space window-manager session presenting
composited frames to a user-space display-driver process, zero-copy,
lease-gated end to end, fast enough for 4K video and games and reusable
by a future remote-control display service (the same protocol served by
a network sink instead of a scanout surface; latency, not architecture,
is the only difference).

**Design (binding).** The shape is deliberately DRM/Wayland-grade:

- **Zero-copy frames.** The session creates one `shm_create` region
  holding two frames (double buffer), renders into the back frame, and
  presents by *index* — no frame bytes ever cross the IPC. The display
  service maps the region **once** at configure time; the hot path
  (present) does no mapping, no allocation, and no copy other than the
  driver's own blit to scanout (a direct-scanout driver may eliminate
  even that by scanning out the granted region itself — the region stays
  mapped, so the protocol already permits it).
- **Framebuffer memory policy is discovered, never guessed.** A linear
  framebuffer resource carries `WriteBack` for coherent RAM (QEMU `ramfb`)
  or `WriteCombine` for a CPU-written aperture. The kernel preserves that
  policy into the page-table mapping; unsupported WC fails closed. The HVS
  fallback and plane uploads use one bulk transfer. Scanout y-wrap remains
  disabled unless a backend explicitly proves wrap or bounded-pan semantics;
  neither current `ramfb` nor the Pi mailbox does.
- **The lease is checked kernel-side, per present, with no oracle.** The
  service never trusts a claimed lease: it asks the kernel whether the
  *in-flight caller of its own endpoint* holds the live lease
  (`call_peer_seat`, below). Facts flow only about a task that already
  called you — the `call_peer_origin` trust shape — so seat ownership is
  never enumerable (`SEAT_LIST` stays `CAP_SYSINFO_HW`).
- **Sharing memory is an explicit, endpoint-directed capability act.**
  `shm_grant` lets a region's owner mint a map-grant **to the serving
  task of an endpoint it can already call** — never to a raw (recyclable)
  PID, so a grant cannot land on a reused task id. The handle value
  travels in-band; it is owner-checked at `shm_map`, so a bystander who
  learns the number holds nothing.
- **Input parks; nothing polls.** The session blocks on its wait-set —
  a new `SeatInput` member kind, woken by input delivery *and by
  revocation*, so a session that lost its seat wakes, observes the typed
  `SeatRevoked` on its next drain, and tears down instead of parking
  forever or scribbling on.
- **Input must never starve the session's other duties.** The session
  handles one woken source per wake, so its seat member sharing a set with
  the window endpoint, the child reaper and the service mailboxes only
  works because `waitset_wait` hands ready members out **in turn** (the
  registry's resume cursor, `docs/src/architecture/syscalls.md`). Under
  first-registered priority a hand on the mouse held the seat member ready
  continuously and nothing else in the set was ever served: applications
  blocked in a window call hung, exits went unreaped, and the queues peers
  post to filled until their sends failed `WouldBlock` — which the tray
  monitor then read as five publish failures and exited on.
- **A full-screen frame ring exceeds the single buddy block.** A shared
  region is backed by a *list* of buddy chunks (`SharedChunk`) mapped into
  one guard-bracketed contiguous virtual window, so the double buffer is
  bounded by RAM, not the 8 MiB single-block ceiling (`MAX_ORDER`). A small
  region stays a single chunk (the USB path is unaffected); `kernel_hold`
  refuses a multi-chunk region (fail closed — no kernel consumer maps one).
  This is what makes `desktop` come up on a real display rather than failing
  with `shared frame region refused`.

**Staged next — driver-owned, exported frame buffer (DMA-BUF).** The frame
ring is still *client-allocated* (`shm_create` + `shm_grant` up to the
service). The approved next tranche inverts this so the **display driver**
allocates/owns/exports the ring (VRAM when the card has its own memory, else a
system chunked region) and the compositor imports it — the DRM/dma-buf shape.
Design + slices are staged in `.junie/next-desktop-prompt.md` (a new IPC
"peer-grant" primitive, VRAM sub-window export, and the `display_ipc` protocol
inversion).

Sub-stages, each shipped complete (code + tests + docs, §7 gate green):

- **D7a — kernel surfaces `[x]` — done.** All three surfaces are live with
  kernel host tests (grant/deny/revoked-window/readiness), `lib/rt`
  wrappers + marshal tests, `tairix_sys_*` stubs, regenerated C headers, and
  the `docs/src/architecture/syscalls.md` / `docs/src/desktop/seat.md`
  pages updated in the same change.
  - `WaitSourceKind::SeatInput` (wire value 3, `id` = seat id):
    owner-checked at `waitset_ctl` add (the caller must hold the seat's
    live lease; `NotFound`/`SeatNotOwner` otherwise). Ready when the
    seat's keyboard **or** pointer channel holds a record, or when the
    member's task no longer holds the live lease (revoke / release —
    wake-on-revoke makes the loss observable). Wake hooks ride the
    existing inject/deliver and revoke/release paths in the kernel seat
    registry; no new polling.
  - `shm_grant` (`abi-v1` 82): `shm_grant(region_id, endpoint_id)` →
    grant-handle. `CAP_SHM`-gated; the caller must own the region; the
    recipient is resolved as the live serving task of `endpoint_id` at
    grant time; fail-closed typed errnos; the mint is audit-logged with
    a stable event id.
  - `call_peer_seat` (`abi-v1` 83): `call_peer_seat(endpoint_id,
    seat_id)` → live lease generation. Valid only between `call_recv`
    and `call_reply` on an endpoint the caller serves (the
    `call_peer_origin` window); returns `SeatNotOwner` / `SeatRevoked` /
    `NotFound` fail-closed. No capability: the authority is serving the
    in-flight call, exactly as `call_peer_origin`.
- **D7b — the display service. `[x]` — done.**
  `lib/abi/src/display_ipc.rs` (the fixed-width, fail-closed,
  fuzzed protocol: `Query` → mode reply, `Configure { shm_handle,
  frame_count, frame geometry }`, `Present { frame_index, damage rect }`,
  every reserved tail zero-checked) and the reserved `DISPLAY_ENDPOINT`
  (`0x0D15_1001`, in `is_reserved_endpoint`; one endpoint — the service
  carries seat ids in-protocol, so a later multi-GPU broker is additive,
  not a v2); the shared `lib/abi::reply` status frame (hoisted from the
  seatmgr module, §2.2); the `lib/display` crate hosting **both** halves
  over injected seams so the protocol semantics have one definition
  (§2.2): the server engine (`DisplayServer`: decode → lease check via
  the `SeatCheck` seam over `call_peer_seat` on **every** request, `Query`
  included — only the seat owner learns the mode → exact-mode geometry
  validation → map-once `ShmMapper` → blit through the `Display` trait;
  the configure state is bound to the granting lease's *generation*, so a
  revoked or re-acquired seat must reconfigure before it can present, and
  an observed lease loss drops the stale mapping) and the client
  (`DisplayClient` over a `DisplayTransport` seam; `RemoteDisplay`
  implements the *existing* `Display` trait over the client's mapping
  with per-frame stale-damage union bookkeeping, so `Compositor::present`
  is unchanged and a double-buffered frame is always current). The
  in-place `Display::present_region` evolution landed with a full-blit
  default, a real partial blit in the framebuffer driver, and the WM
  compositor threading its composited damage bounds through it. The
  surface-discovery contract landed as `HwResourceKind::Framebuffer`
  (the FDT `simple-framebuffer` model normalised into the hardware tree,
  §18.1): a geometry-carrying, `CAP_MMIO_MAP`-gated scan-out window with
  a validated `framebuffer_mode` decode, the `sole_framebuffer` grant
  resolver, and `mmio_map` admission — plus the distinct
  `Errno::DeviceFault` (`DriverError::as_errno` now maps
  `DeviceFault`/`Busy` to `DeviceFault`/`WouldBlock`).
  **The framebuffer service process** hosts the engine: the
  linear-surface engine (`Framebuffer`/`FramebufferConfig`) lives in
  `lib/display` (`lib/display/src/framebuffer.rs`, tests in
  `lib/display/tests/framebuffer.rs`; the three framebuffer QEMU
  verticals drive it as legal non-driver consumers);
  `drivers/display/framebuffer` is the bin-only `Run` crate (build.rs
  `freestanding` cfg, the shared `lib/rt/Run.ld`, host stub — the
  `virtio_kbd` shape)
  wiring `RtDriverHost` grants → `sole_framebuffer` → surface, the
  reserved `DISPLAY_ENDPOINT` bind under `CAP_IPC_BIND_PRIVILEGED`, an
  `RtSeatCheck` over `call_peer_seat`, and an `RtShmMapper` over
  `shm_map` into a waitset-parked `DisplayServer::serve` loop
  (fail-loud reserved exit codes; never a busy poll). `shm_map`
  (`abi-v1` 41) reports the mapped region's byte length through a
  `len_out` user pointer — the kernel registry's own record, so a
  server (and the four shm-consuming driver programs, which now verify
  it before building their slices) sizes its view from the kernel's
  answer, never the granting task's claimed geometry. The service's
  image bundle + bind keys land with the D7d autoload world.
- **D7c — the desktop session binary. `[x]` — done.**
  `userland/gui/session` ships the `Run` program (`src/run.rs`, the
  login-crate lib+bin shape: `freestanding` build.rs cfg, the shared PIE
  `Run.ld`, host stub elsewhere) as the `desktop` **application** — its
  `AppInfo.toml` (kind `application`, the AW3/AW5/CU6-grown request pinned in
  the kernel registry tests) plants it in the system application store:
  `display_acquire(SEAT_PRIMARY)` → `DisplayClient` bring-up over
  `ipc_call` (query → checked frame arithmetic → `shm_create` double
  buffer → `shm_grant` to the display endpoint's serving task →
  configure) → `RemoteDisplay` over the session's own mapping → the live
  `SeatEventReader`s over `tairix_rt::pointer_read`/`keyboard_read`
  drained after each `SeatInput` wake → `DesktopShell` pump → composite →
  present with damage. `DeviceInputSource` receives the screen `Rect`
  from the queried mode; the desktop layer carries the pinboard — the
  user's wallpaper, or the backdrop colour their settings name, with
  their `Desktop` folder's icons over it (`plans/PINBOARD.md`). Loss of
  the seat (typed
  `SeatRevoked`/`SeatNotOwner` on any drain or present) tears the
  session down fail-loud — reason on `stderr`, reserved exit codes
  90–97, owner-checked `display_release` on every exit path — never a
  spin or a blind repaint. The bundle's image planting and spawn ride
  D7d.
- **D7d — end to end. `[x]` — done.** The autoload QEMU vertical world
  is a *display* world: the aarch64 boot publishes the ramfb scan-out
  surface as a boot display node (a `HwResourceKind::Framebuffer` grant
  + `simple-framebuffer` match key), the signed framebuffer-service
  bundle is discovered in the on-volume store and spawned onto that
  node's grants, and the vertical proves — with the whole dialogue typed
  at the seat keyboard, since the video console is the only console —
  the typed passphrase unlocking the root, both per-kind
  `INPUT_DELIVERED` witnesses from the autoloaded user-space input
  drivers, and the service's `DISPLAY_ENDPOINT` bind under
  `CAP_IPC_BIND_PRIVILEGED`. Landed with the first stage: the drvrt host
  maps `Framebuffer` grants; the per-task MMIO/shared window ceilings
  are the reserved 1 GiB spans with lazily grown, fail-closed
  bookkeeping; and fixture build scripts register the inner build's
  dep-info (`tests/integration/harness` `dep_info`).
  **D7d-2 (the desktop launch) guarantees:**
  - The desktop is the `desktop` **application** in the system application store
    (`userland/gui/session`, bundle `desktop.app`): the shell resolves
    the bare command word `desktop` to it, and a graphical login spawns
    the same bundle — one bundle, one spelling.
  - Session selection is system policy, never a per-login prompt: login
    starts the account's shell unless `os.loginType graphical` is
    configured (`lib/sysconfig`) *and* the per-round probe holds — a
    read-only `fs_open` of `DESKTOP_SESSION_PATH`
    (`tairix_login::session`, the one spelling of
    `/System/Applications/desktop.app/Run`) plus one `Query` `ipc_call` to the
    reserved `DISPLAY_ENDPOINT` (any well-formed reply proves a
    privileged-bound service; `NotFound` proves none); a configured
    graphical default that cannot start degrades to text, never an
    error. A graphical login spawns the D7c session as the
    authenticated user (`session_program`). Login's manifest gained
    `CAP_FS_ACCESS` for exactly that probe (AppInfo + kernel pin in
    lockstep).
  - The CU6 ceiling slice: `SESSION_BASELINE` carries the
    graphical-session class (`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`),
    so `desktop.app`'s manifest survives the `manifest ∩ ceiling`
    intersection for every interactive account; the shell's manifest was
    decoupled to its own exercised set (`SHELL_MANIFEST`) so the wider
    baseline never widens elsh.
  - The display service's engine latches `has_presented()` and the `Run`
    binary emits the one-shot `FIRST_PRESENT` record (`EventId` 15001,
    range `15000..16000` in the driver crate's lib target, message
    shared with every consumer; manifest += `CAP_LOG_EMIT` at both plant
    sites) after the first successful client present — off the hot path.
  - The vertical types `root`/`root` + the `desktop` command at the
    shell the text login drops to (a second marker-gated typed-keys step
    keyed on the `UsersDbLoaded` serial witness; the dialogue is
    compile-time-pinned to the fixture credentials), and the
    runner keys **both** a QEMU-monitor screendump and the mouse
    injection on the `FIRST_PRESENT` marker, ordered present → fully
    parsed dump → pointer → `kind=pointer` witness → PASS, then proves
    the composited frame reached scan-out by **recomputing the desktop's
    own wallpaper on the host** — the shipped default master through the
    same decode, placement and resampling code the guest runs — and
    requiring exact equality at sampled points clear of the taskbar,
    the pointer, the icon column and any served window
    (`plans/PINBOARD.md`). Nothing is asserted from a literal colour:
    a boot console, a blank frame, or a wrong fit or scale all differ
    and are refused (`tools/qemu` grew the typed-keys script, the
    verified `Screendump` step, and the fail-closed PPM decoder
    `screendump::parse_ppm`; an unverified requested dump fails the run
    even on a guest PASS).
  `plans/PI.md` P10's final step rides this landing.

**Explicitly not in D7:** a GPU/3D or video-decode pipeline (the
protocol's damage + direct-scanout shape is designed so those extend it
in place); a network display service (same protocol, later plan); the
input-device→seat topology policy (CU6 / `plans/PI.md` P11).

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
