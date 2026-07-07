# `rustos-seatmgr` — seat-manager service

`plans/DISPLAY.md` Stage D3 deliverable. The user-space service that holds
`CAP_SEAT_ADMIN` — the seat-multiplexing authority (the `chvt`/`logind`
analogue: switch which session is foreground across every seat, forcibly
revoke a wedged owner's lease). Installed to
`/System/Services/seatmgr.app/Run`; headless-safe (no `userland/gui/*`
edge).

Stability tier: `experimental`.

## What this crate is

The **dispatcher** — the policy layer. It owns no seat state of its own
(the one seat state machine is `lib/seat`, hosted by the kernel's seat
registry). For each request `serve` performs, in order, failing closed at
the first problem:

1. Decode the `SeatAdminRequest` (`rustos_abi::seat`, `seatmgr-v1`).
2. Require the requester's kernel-attested `Origin` to carry
   `CAP_SEAT_ADMIN` **before** touching any state. The broker never
   launders its own capability onto an unprivileged caller, and the
   kernel re-checks the capability and every seat/console index when the
   syscall is issued — the service adds audited policy without widening
   reach.
3. Forward the operation through the `SeatAdmin` seam (`seat_switch` /
   `seat_revoke` in production) and audit the outcome.

## Operations served (`seatmgr-v1`)

| Operation | Syscall       | Kernel refusals                              |
|-----------|---------------|----------------------------------------------|
| `Switch`  | `seat_switch` | `NotFound` (unknown seat/console)             |
| `Revoke`  | `seat_revoke` | `NotFound` (unknown seat), `SeatNotOwner` (unowned) |

Every applied operation, refusal, and malformed request is logged with a
stable event id in the `14000..15000` range (`src/events.rs`); the kernel
additionally audits each switch/revoke itself (events 4051/4052), the
revoke record carrying the evicted owner's task id.

## Endpoint

The reserved rendezvous `SEATMGR_ENDPOINT` (`rustos_abi::seat`) is
squat-protected: binding it requires `CAP_IPC_BIND_PRIVILEGED`
(`rustos_abi::ipc::is_reserved_endpoint`). Replies are the 4-byte status
frame (`0` or a negative `Errno`).

## Testing

`cargo test -p rustos-seatmgr` drives `serve` against in-memory fixtures:
authorised switch/revoke forwarding + audit, unprivileged denial before
any state, kernel-refusal pass-through, malformed-request rejection, and
the event-id range/uniqueness pins.
