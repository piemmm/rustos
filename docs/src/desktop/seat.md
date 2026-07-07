# Seat ownership

A **seat** is one physical display plus the keyboard and pointer attached to
it. Seat ownership decides which task owns that surface, how the ownership is
granted, released, and forcibly revoked, and where the seat's input is routed
at every moment. The staged design lives in `plans/DISPLAY.md`; this page
describes what is implemented.

## The model (`lib/seat`)

`rustos_seat` is the arch-neutral, dependency-free, `no_std` state machine
behind seat ownership — the single definition the in-kernel seat registry and
the user-space seat manager will both build on (Stages D2–D6 of
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
(`rustos_kernel_core::seat::SeatRegistry`): one seat per running kernel
today (multi-seat is Stage D6), holding the `SeatState` under the
registry's own lock next to the two input sinks it routes between — the
foreground text console's type-ahead queue and the bounded desktop
keyboard channel (which zeroes each record as it is drained, so a typed
secret never lingers).

- `display_acquire` (`abi-v1` 23, `CAP_DISPLAY`) records the
  kernel-attested calling task as the seat owner. A seat held by another
  task refuses the claim with `Errno::SeatBusy`; a repeat acquire by the
  holder is surfaced as `Errno::AlreadyExists`.
- `display_release` (`abi-v1` 24, `CAP_DISPLAY`) is owner-checked: a
  caller that does not hold the seat is refused with
  `Errno::SeatNotOwner` (`Errno::SeatRevoked` once, after an
  administrative eviction) and the owner keeps the seat.
- `keyboard_read` (`abi-v1` 25, `CAP_INPUT_READ`) is owner-gated through
  `SeatState::access`: only the seat owner drains the desktop keyboard
  channel, so a second capability holder can never siphon another
  session's keystrokes.
- Routing follows `SeatState::route`: a held seat's key edges go to the
  owner's desktop channel, an unowned seat's to the foreground text
  console — a released seat returns the keyboard to the text login
  immediately.

Both `display_*` calls are audited per call (a seat hand-over is the
analogue of a foreground-tty switch), and every refusal is a typed
`Errno`, mapped from `SeatError` in exactly one place
(`rustos_kernel_core::seat::seat_errno`).

## What is not yet wired

`CAP_SEAT_ADMIN` with the `seat_switch`/`seat_revoke` syscalls and the
`seatmgr` service, the lease-derived present right, per-console
controlling-terminal arbitration, and multi-seat/hotplug are Stages D3–D6
of `plans/DISPLAY.md`.
