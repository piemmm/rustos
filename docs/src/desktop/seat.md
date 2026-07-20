# Seat ownership

A **seat** is one physical display plus the keyboard and pointer attached to
it. Seat ownership decides which task owns that surface, how the ownership is
granted, released, and forcibly revoked, and where the seat's input is routed
at every moment. The staged design lives in `plans/DISPLAY.md`; this page
describes what is implemented.

## The model (`lib/seat`)

`tairix_seat` is the arch-neutral, dependency-free, `no_std` state machine
behind seat ownership — the single definition the in-kernel seat registry and
the user-space seat manager both build on (Stages D2–D6 of
`plans/DISPLAY.md`).

One seat is a `SeatState`: a **lease** plus a **foreground text console**.

- **Acquire** (`SeatState::acquire`): grants the seat to the kernel-attested
  caller (`SeatOwner` — the kernel's task identity, never a caller-supplied
  claim) and mints a `Lease`. A seat held by another task refuses the acquire
  with `SeatBusy` — ownership is never displaced — and a double acquire by
  the holder is surfaced as `AlreadyOwner` rather than silently succeeding.
- **Release** (`SeatState::release`): only the recorded owner may release;
  anyone else is refused with `NotOwner`. This is what makes "an ordinary
  task cannot steal focus" an enforced invariant rather than a documentation
  claim.
- **Revoke** (`SeatState::revoke`): the administrator path (the seat
  manager's `CAP_SEAT_ADMIN` authority, Stage D3) evicts the current owner,
  returning the evicted identity for the audit log. Revocation is
  **observable**: the evicted task's next owner-gated call is refused with
  the distinct `SeatRevoked`, so a well-behaved compositor learns it lost the
  seat instead of scribbling over the new foreground. A fresh acquire —
  including an explicit reacquire by the evicted task — clears the marker: an
  acquire is a new, capability-checked claim.
- **Lease generations**: every grant carries a per-seat monotonic
  generation, so a lease that survived a revoke/reacquire cycle can never be
  confused with the live one. Stage D4 derives the framebuffer present right
  from this live lease.
- **Routing** (`SeatState::route`): a held seat's key edges go to the owner's
  desktop channel; an unowned seat's — including immediately after a revoke —
  go to the seat's foreground text console, never to a stale desktop channel.
- **Owner-gated access** (`SeatState::access`): the check a present or
  desktop-keyboard-drain path applies against the live lease.

Every transition is total and fail-closed: an illegal request returns a typed
`SeatError` and leaves the seat unchanged; no path panics. The type is a pure
value — capability checks happen in the kernel before its methods are
reached, and the registry hosting it owns the synchronisation.

## What the kernel enforces (Stage D2)

The kernel hosts this state machine in its seat registry
(`tairix_kernel_core::seat::SeatRegistry`): every seat on the machine,
each holding its own `SeatState` under its own lock next to the input
sinks it routes between — the seat's foreground text console type-ahead
queue and its bounded desktop keyboard and pointer channels. The two
desktop channels share one ring definition (`InputChannel`), differing
only in capacity (64 key records; 256 pointer records — a pointing device
emits far more events between per-frame drains) and both zero each record
as it is drained, so a typed secret never lingers. Every seat-addressed
syscall names its seat and fails closed with `Errno::NotFound` for one
that does not (or no longer) exist.

- `display_acquire` (`abi-v1` 23, `CAP_DISPLAY`) records the
  kernel-attested calling task as the named seat's owner and returns the
  minted lease's generation (`>= 1`) — the client-visible handle the
  present right is derived from (Stage D4 below). A seat held by another
  task refuses the claim with `Errno::SeatBusy`; a repeat acquire by the
  holder is surfaced as `Errno::AlreadyExists`.
- `display_release` (`abi-v1` 24, `CAP_DISPLAY`) is owner-checked: a
  caller that does not hold the seat is refused with
  `Errno::SeatNotOwner` (`Errno::SeatRevoked` once, after an
  administrative eviction) and the owner keeps the seat.
- `keyboard_read` (`abi-v1` 25, `CAP_INPUT_READ`) is owner-gated through
  `SeatState::access`: only the seat's owner drains its desktop keyboard
  channel, so a second capability holder — even the owner of *another*
  seat — can never siphon this session's keystrokes.
- `key_inject` (`CAP_INPUT_INJECT`) carries the seat the decoded key
  edge belongs to (the seat whose keyboard produced it; the boot seat
  for a directly attached keyboard), and routing follows that seat's
  `SeatState::route`: a held seat's key edges go to its owner's desktop
  channel, an unowned seat's to its foreground text console — a released
  seat returns the keyboard to the text login immediately.
- `pointer_inject` (`abi-v1` 78, `CAP_INPUT_INJECT`) and `pointer_read`
  (`abi-v1` 79, `CAP_INPUT_READ`) are the pointer analogues: a
  pointer-input driver injects each decoded relative motion or resolved
  button edge for its device's seat (a `PointerInput` record is
  screen-independent — the seat owner, which owns the compositor,
  accumulates displacements into the on-screen position; a scroll wheel
  rides the same channel as a `Scrolled` tick record), the registry
  queues it on a held seat's pointer channel, and only the live lease
  owner drains it — the same `SeatState::access` gate as
  `keyboard_read`, so no other capability holder can observe the pointer
  stream. While the seat is unowned the record is consumed and
  discarded: the text console has no pointer consumer, and the driver
  never learns — and never chooses — the destination. The first
  delivered record of each input kind emits that kind's one-shot
  `INPUT_DELIVERED` witness (`kind=key` / `kind=pointer`), so keyboard
  and pointer liveness are separately attributable from the log.
  Desktop input is deliberately *not* a named IPC port: a port's receive
  gate is capability-only and cannot express "only the live seat-lease
  holder may drain".

Both `display_*` calls are audited per call (a seat hand-over is the
analogue of a foreground-tty switch), and every refusal is a typed
`Errno`, mapped from `SeatError` in exactly one place
(`tairix_kernel_core::seat::seat_errno`).

## Seat administration (Stage D3)

The seat-multiplexing authority — the `chvt`/`logind` analogue — is the
single new capability **`CAP_SEAT_ADMIN`** (id 33), enforced by two
audited syscalls and held by exactly one service:

- `seat_switch` (`abi-v1` 70, `CAP_SEAT_ADMIN`) retargets which installed
  text console an unowned seat's input drains to. The seat id and the
  console index are validated against the live
  topology **before** any state changes — an unknown either fails closed
  with `Errno::NotFound`, so a typo can never strand input on a console
  that does not exist. A held seat keeps routing to its owner until the
  lease ends. Every switch is audited (`SEAT_SWITCHED`, event 4051, with
  the seat and the new foreground).
- `seat_revoke` (`abi-v1` 71, `CAP_SEAT_ADMIN`) forcibly evicts the
  current lease holder through `SeatState::revoke`. An unknown seat fails
  closed with `Errno::NotFound`; an unowned seat refuses with
  `Errno::SeatNotOwner` (there is no lease to revoke). On success the seat
  is immediately acquirable, input returns to the text foreground, and the
  evicted owner's next owner-gated call fails closed with the distinct
  `Errno::SeatRevoked`. Every eviction is audited (`SEAT_LEASE_REVOKED`,
  event 4052) **with the evicted owner's task id**, so every eviction is
  attributable.
- **`seatmgr`** (`userland/system/seatmgr`, installed at
  `/System/Services/seatmgr.app/Run`, launched by PID 1) is the sole
  manifest holder of `CAP_SEAT_ADMIN`. It binds the reserved
  `SEATMGR_ENDPOINT` rendezvous (`tairix_abi::seat`, squat-protected by
  the `CAP_IPC_BIND_PRIVILEGED` gate) and serves the typed
  `SeatAdminRequest` operations, requiring each *requester's*
  kernel-attested origin to itself carry `CAP_SEAT_ADMIN` before the
  syscall is issued — the broker adds audited policy without laundering
  its own authority onto an unprivileged caller, and the kernel re-checks
  the capability and every index on each call. Headless-safe: nothing in
  it depends on a graphical session.

## The present right follows the live lease (Stage D4)

Mapping the framebuffer (`CAP_MMIO_MAP`) and owning the seat
(`CAP_DISPLAY`) are separate facts; Stage D4 couples them at the display
driver's present path, so "I can write pixels" no longer implies "I own
the screen":

- The `lib/abi` seat handle is `tairix_abi::seat::SeatLease` — seat id,
  owning task, and the mint-time generation `display_acquire` returned.
  The generation is what makes a stale pre-revoke handle refusable even
  after its owner reacquires the seat: `tairix_seat::SeatState::verify`
  (the one definition of the check) accepts exactly the live
  owner-and-generation pair.
- The check reaches a display driver through its host:
  `DriverHost::seat_gate()` returns the `SeatGate` the kernel bound to
  the presenting client's lease (`SeatRegistry::present_gate`), and every
  driver (`framebuffer`, `vesa`, `rpi_hvs` — both its software present
  and its hardware `present_layers` flip) consults it **first**, before
  any validation or surface access. The gate re-reads the registry's
  live lease on every call; it caches nothing.
- A revoked client's present is refused with the distinct
  `DriverError::SeatRevoked` (mapping to `Errno::SeatRevoked`), so a
  well-behaved compositor learns it lost the seat; any other dead handle
  (unowned seat, another owner, stale generation, foreign seat id) is a
  plain `DriverError::PermissionDenied`. Either way the refused frame
  never touches the scan-out surface, even though the client's
  framebuffer mapping still exists.
- A host with no seat wired — a headless build or a boot-console
  bring-up surface — exposes no gate, and the driver presents ungated:
  there is no lease to derive the right from.

The aarch64 framebuffer QEMU vertical proves the property end to end on
a real kernel seat registry: the owner presents, an administrative
revoke evicts it, the evicted client's next present is refused (its last
frame stays on scan-out), and the new foreground's fresh lease renders.

## Multi-seat and hotplug (Stage D6)

A machine with several displays is several **independent seats**, one
uniform kernel object each — its own owner, lease generations, foreground
console, input routing, and present gate — all reusing the one `lib/seat`
state machine:

- **The boot seat (id 0, `SEAT_PRIMARY`) always exists**, even headless,
  where it is a text-only seat; its text sink is the console that owns
  the directly attached keyboard. Every further seat is minted by
  hardware discovery.
- **Hotplug rides the one discovery path.** A display-class node
  published into the live hardware tree (`hw_emit_node`) mints a seat
  for it, and the node's removal (`hw_remove_node`, including as a
  removed subtree descendant) destroys it — no reboot, no parallel
  device list. Both topology changes are audited with the seat and node
  ids (`SEAT_CREATED` 4053, `SEAT_DESTROYED` 4054).
- **Seat ids are minted monotonically and never reused**, so a stale
  lease, handle, or record can never alias a later seat — even the same
  display replugged mints a fresh id.
- **A destroyed seat fails closed everywhere, instantly.** Every call
  naming the dead seat — acquire, release, inject, drain, switch, revoke
  — refuses with `Errno::NotFound`, and a still-held lease's present
  gate refuses the very next frame (the gate re-resolves the seat on
  every call). The dead seat's keyboard channel is zeroed as it is
  freed, so an undrained keystroke never outlives its seat.
- **Independence is enforced, not assumed.** One seat's acquire, revoke,
  or input never touches another's; each seat's channel is drained only
  by its own owner (cross-seat drains are `SeatNotOwner`).

The QEMU vertical's multi-seat phase proves two seats with independent
owners and input routing on a real registry, presents under the hotplug
seat's lease, then detaches the display and shows the dead seat's lease
refused while the boot seat is untouched; kernel host tests drive the
same lifecycle through the real `hw_emit_node`/`hw_remove_node`
handlers.

## Per-console controlling owner (Stage D5)

Text consoles get the controlling-terminal arbitration Linux builds from
session leaders, foreground process groups, and `SIGTTIN`/`SIGTTOU` — but
as a kernel-tracked fact with fail-closed refusals instead of racy
asynchronous signals.

- **The controlling owner is a kernel-tracked task id per console.** While
  an owner is recorded, **only the owner** drains that console's input
  queue (`stream_read`) or changes its line discipline
  (`stream_input_mode`); every other task — including the granting shell
  itself — is refused with the typed `Errno::NotForeground` (27) *before
  any input is consumed*. Two tasks on one console can never both drain
  it. An unowned console reads openly (the shell at its prompt;
  single-tenant bring-up), exactly as before.
- **Handoff is an explicit, checked call, and moves only down the spawn
  chain.** `console_foreground` (72, `CAP_CONSOLE_READ`) grants the
  ownership to a **live child of the caller** (the same
  `ProcessWait::authorise_child` bookkeeping `wait`/`signal` use —
  inherited and intersected, never widened) and records the caller as the
  **granter**. The slot transition itself is owner-checked on the console
  device: a grant is honoured only from an unowned console, the recorded
  granter (re-targeting between its own children), or the current owner
  (delegating onward to its own child); a release (`pid = 0`) only from
  the granter or the owner. A bystander can neither take the drain right
  nor open the console by clearing the slot — both are refused with
  `NotForeground`.
- **No wedged consoles.** A vanished owner never strands its console: the
  `exit` path releases any console ownership the exiting task held, and
  the read gate clears a recorded owner the process bookkeeping proves
  dead (`ProcessWait::is_live`; task ids are never reused, so a proven
  death is final). The inert bookkeeping default reports every task live,
  so a gate that cannot prove death keeps denying — it can heal, never
  widen.
- **Signal routing rides the same slot.** The cooked-mode `^C`/`^Z`
  delivery to the foreground job (SP9) targets the same recorded owner, so
  "who gets the interrupt" and "who drains the input" can never diverge.

Kernel host tests prove the exclusivity (two tasks cannot both drain, the
refused reader consumes nothing), the handoff transfer, the bystander
steal refusals, the input-mode gate, and both no-wedge paths (exit release
and gate healing).

## Toward the live desktop session (Stages D7a–D7c)

Stage D7 (`plans/DISPLAY.md`) is the display-client present path: a
user-space window-manager session presenting composited frames to a
user-space display-driver service, zero-copy and lease-gated end to end.
Its kernel surfaces are live:

- **Park on seat input, never poll.** The wait-set accepts a `SeatInput`
  member (`waitset_ctl`, kind 3, `id` = seat id), owner-checked at add
  against the seat's live lease with the same oracle-free `NotFound` the
  other member kinds use. The member is ready when the seat's keyboard
  **or** pointer channel holds a record — and when the caller *loses* the
  lease (release, administrative revoke, display hot-removal), so a
  parked session wakes, observes the typed `SeatRevoked`/`SeatNotOwner`
  on its next drain, and tears down instead of parking forever. The wake
  rides the registry's inject and revoke paths; only sets holding a
  `SeatInput` member join the seat wake queue, so pointer-rate wakes
  never touch unrelated waiters.
- **The per-present check is `call_peer_seat`** (`abi-v1` 83): while a
  present request is in service (between `call_recv` and `call_reply`),
  the display service asks the kernel whether the *in-flight caller*
  holds the named seat's live lease and receives the lease generation or
  the typed refusal. The trust shape is exactly `call_peer_origin` — a
  server learns seat facts only about a task it is actively servicing —
  so seat ownership is never enumerable, and the answer is fresh at
  check time exactly like the kernel-side present gate.
- **Frames travel by grant, not by copy.** The session shares its frame
  region with the display service through `shm_grant` (`abi-v1` 82): an
  endpoint-directed, `CAP_SHM`-gated, audited delegation that mints the
  endpoint's live serving task its own unforgeable `shm_map` handle —
  never a raw (recyclable) PID, and never a handle a bystander could
  use.

On top of those surfaces, the display-service protocol and its engine
are live (stage D7b):

- **The wire protocol** is `tairix_abi::display_ipc`: one reserved,
  squat-protected rendezvous (`DISPLAY_ENDPOINT`, bindable only under
  `CAP_IPC_BIND_PRIVILEGED`) serving the fixed-width, fail-closed
  requests `Query` (→ the mode reply), `Configure { shm grant handle,
  frame count, frame geometry }`, and `Present { frame index, damage
  rect }`. Requests carry the seat id in-protocol; every reserved tail
  is zero-checked, and the decoders are fuzzed alongside the other ABI
  decoders.
- **One definition of the semantics.** The `lib/display` crate hosts
  both halves over injected seams: the `DisplayServer` engine a display
  driver's `Run` binary hosts (decode → `call_peer_seat` lease gate on
  **every** request, `Query` included, so only the seat owner learns
  the mode → exact-mode geometry validation → map-once shm adoption →
  bounded, damage-aware blit through the `Display` trait) and the
  `DisplayClient`/`RemoteDisplay` client the desktop session presents
  through — `RemoteDisplay` implements the existing `Display` trait
  over the session's own mapping of the shared frame region, tracking
  each frame's stale region so a double-buffered present copies only
  the pixels that need refreshing.
- **Frames are bound to the lease that configured them.** `Configure`
  records the granting lease's generation; a `Present` under any other
  generation (a revoked or re-acquired seat) is refused until the new
  owner reconfigures, and a caller observed to have lost its lease has
  its stale frame region dropped — one owner's frames can never scan
  out under another's lease.
- **Damage travels end to end.** `Display::present_region` (an in-place
  evolution with a full-blit default) carries the changed rectangle
  from the compositor — which reports its composited damage bounds —
  through the protocol to the driver's partial blit, so a small update
  touches only its own scanlines on the scan-out path.
- **A display's surface is discovered, never assumed.** A display-class
  hardware node can now carry a `Framebuffer` resource — a
  geometry-carrying, `CAP_MMIO_MAP`-gated scan-out window (the FDT
  `simple-framebuffer` model normalised into the hardware tree) whose
  validated mode the autoloaded display service reads through
  `sole_framebuffer` before mapping the window through `mmio_map`.

Both `Run` binaries hosting those halves are live (stages D7b–D7c):

- **The framebuffer service process** (`drivers/display/framebuffer`)
  resolves its granted scan-out surface through `sole_framebuffer`,
  binds the reserved `DISPLAY_ENDPOINT` under `CAP_IPC_BIND_PRIVILEGED`,
  and serves the engine from a waitset-parked loop — never a busy poll —
  with fail-loud reserved exit codes.
- **The desktop session process** (`userland/gui/session`, stage D7c)
  is the client half: it acquires the boot seat's lease
  (`display_acquire`), performs the bring-up handshake (query →
  `shm_create` the double-buffered frame region → `shm_grant` it to the
  display endpoint's serving task → configure), and then runs the
  desktop shell from a `SeatInput` wait-set park: each wake drains the
  owned seat's pointer and keyboard channels through the fail-closed
  record path, pumps the decoded events through the compositor and
  taskbar, and presents the composited damage by frame index. Losing
  the seat — the typed `SeatRevoked`/`SeatNotOwner` on any drain or
  present — tears the session down fail-loud (reason on `stderr`, a
  reserved exit code, an owner-checked release); it never spins or
  repaints without a live lease.

Stage D7d (`plans/DISPLAY.md`) proves the whole chain end to end in the
autoload QEMU vertical: the boot display node, the autoloaded framebuffer
service, the `root`/`root` login typed at the seat keyboard followed by
the `desktop` command at the text shell it drops to (the desktop is the
system app store's `desktop.app`, the same bundle a configured
`os.loginType graphical` login spawns directly), the spawned
session's first present — witnessed by the service's one-shot
`FIRST_PRESENT` record — and a host-side QEMU screendump asserted to be
dominated by the theme's desktop colour, so the composited frame
demonstrably reached the scan-out surface.

## Observing seats

The seat inventory is exposed through the System Information API — never
a `/proc`-style file. The `SEAT_LIST` query (`sysinfo-v1` id 12, gated on
`CAP_SYSINFO_HW` and audited, like the hardware tree) returns one
`SeatRecord` per seat: seat id, the owning task (with an explicit
owned/unowned flag — an unowned record carries no owner), the monotonic
lease generation, and the foreground console. The kernel serves the
underlying `IntrospectDomain::Seats` snapshot directly from its seat
registry, paging by whole record — the boot seat first, then every
discovery-created seat in creation order; the `sysinfod` broker scopes
and audits the query, and `sysinfo seats` renders the table.
