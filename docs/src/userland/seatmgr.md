# Seat-manager service

`rustos-seatmgr` is the user-space service that holds `CAP_SEAT_ADMIN` —
the seat-multiplexing authority (`plans/DISPLAY.md` D3): the
`chvt`/`logind`-class power to switch which session is foreground across
every seat and to forcibly revoke a wedged owner's lease. The installed
binary lives at `/System/Services/seatmgr.app/Run`, launched by PID 1 at
startup; it is headless-safe (no `userland/gui/*` edge). The seat model
itself — leases, revocation, routing, the two syscalls, and the audit
events — is documented in [Seat ownership](../desktop/seat.md).

## What the service does

`seatmgr` binds the reserved `SEATMGR_ENDPOINT` rendezvous
(`rustos_abi::seat`; binding a reserved id requires
`CAP_IPC_BIND_PRIVILEGED`, so a squatter cannot intercept
seat-administration traffic) and serves the fixed-width, versioned
`SeatAdminRequest`:

| Operation | Forwards to   | Kernel refusals                                     |
|-----------|---------------|-----------------------------------------------------|
| `Switch`  | `seat_switch` | `NotFound` (unknown seat/console)                   |
| `Revoke`  | `seat_revoke` | `NotFound` (unknown seat), `SeatNotOwner` (unowned) |

The reply is a 4-byte status frame: `0`, or the negative `Errno` of the
refusal.

## The gate

For each request the dispatcher (`rustos_seatmgr::serve`), in order and
failing closed at the first problem:

1. Decodes the request against `seatmgr-v1` (unknown magic, version,
   operation, or a dirty reserved field is refused).
2. Requires the *requester's* kernel-attested `Origin` — read through
   `call_peer_origin`, never from the payload — to itself carry
   `CAP_SEAT_ADMIN`. The broker never launders its own capability onto an
   unprivileged caller; the endpoint is the policy surface and the syscall
   the mechanism, both gated on the one capability.
3. Forwards the operation through the `SeatAdmin` seam and audits the
   outcome with a stable event id (`14000..15000` range): applied,
   denied, or malformed. The kernel additionally audits every switch and
   revoke itself (events 4051/4052), the revoke record carrying the
   evicted owner's task id.

The kernel re-checks `CAP_SEAT_ADMIN` and validates the seat and console
indices on every forwarded syscall, so even a compromised service cannot
exceed the kernel's own gate.

## Testing

`cargo test -p rustos-seatmgr` drives `serve` against in-memory fixtures:
authorised switch/revoke forwarding and audit, unprivileged denial before
any state is touched, kernel-refusal pass-through, malformed-request
rejection, and the event-id range/uniqueness pins. The wire decoders are
additionally hammered by the `lib/abi` fuzz harness
(`cargo xtask fuzz`, target `rustos-abi/fuzz_decode`).
