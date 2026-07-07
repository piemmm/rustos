# rustos-seat

The arch-neutral **seat** model for RustOS (`lib/seat`, `plans/DISPLAY.md`
Stage D1).

A seat is one physical display plus the keyboard and pointer attached to it.
This crate is the single definition of the state machine behind seat
ownership:

- `SeatState` — one seat's complete state: the lease and the foreground
  text console.
- `acquire` / `release` — the owner-checked hold on the seat. An acquire is
  refused (`SeatBusy`) while another task holds it — ownership is never
  displaced — and a release by anyone but the recorded owner is refused
  (`NotOwner`).
- `revoke` — the administrator path that evicts a wedged or switched-away
  owner. Revocation is observable: the evicted task's next owner-gated call
  is refused with the distinct `SeatRevoked`, so a compositor learns it lost
  the seat instead of scribbling over the new foreground.
- `Lease` — the granted hold, carrying a per-seat monotonic generation so a
  grant that survived a revoke/reacquire cycle can never be confused with an
  earlier one.
- `route` — the input-routing decision: a held seat's key edges go to the
  owner's desktop channel; an unowned seat's (including immediately after a
  revoke) go to the seat's foreground text console, never to a stale desktop
  channel.
- `access` — the owner-gated check a present/keyboard-drain path applies
  against the live lease.

Every transition is total and fail-closed: an illegal request returns a
typed `SeatError` and leaves the seat unchanged; no path panics.

## Where it sits

`lib/seat` is `no_std`, dependency-free, and `#![forbid(unsafe_code)]`. It is
pure mechanism: capability checks (`CAP_DISPLAY`, `CAP_SEAT_ADMIN`) happen in
the kernel before these methods are reached, identity arrives as the
kernel-attested `SeatOwner` (never a caller-supplied claim), and the kernel's
seat registry owns the locking around each hosted `SeatState`. The in-kernel
registry and the user-space seat manager both build on this one state
machine (`plans/DISPLAY.md` Stages D2–D6); neither re-derives it.

See `docs/src/desktop/seat.md` for the full model and staging.

## Stability

Tier: `experimental`.
