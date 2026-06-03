# Kernel IPC subsystem

`kernel/ipc` is the in-kernel home of the capability-checked primitives
every higher-level kernel and userland component uses to talk to
another address space:

1. **Message ports** — typed endpoints declared via `lib/abi`. Each port
   carries a `required_send_caps`, a `required_recv_caps`, and a
   `max_payload` cap. The kernel enforces the send-side capability set
   on **every** send; the receiver does not re-check (`AGENTS.md` §5.2
   final bullet).
2. **Shared-memory objects** — explicit, capability-gated allocations
   tracked through `kernel/mem`. Revocation tears down every live
   mapping atomically.
3. **Asynchronous notifications** — signal-like bitfields with the
   same capability gates as ports.

Per the Stage 2.5 brief, the crate depends only on `kernel/{sync, mem,
sched, sec}` and `lib/{abi, caps, log, util}`. The Stage 2.7 syscall
dispatcher is out of scope here and consumes this crate's public API.

## Message ports

```text
+--------+   send(payload)    +-----------------+   recv()    +----------+
| sender |  -------------->   |     Port        |  -------->  | receiver |
+--------+   caps checked     | required_send,  |             +----------+
                              | required_recv,  |
                              | max_payload,    |
                              | bounded mailbox |
                              +-----------------+
```

`Port::create` performs the bind-time check (creator must hold every
capability in `required_recv_caps`; restricting senders additionally
requires `CAP_IPC_BIND_PRIVILEGED`). `Port::send` is the per-call
fast path:

1. **Lock-free closed-state check.** An `AtomicU32` is consulted before
   the mailbox lock; sends to destroyed ports return `Errno::NotFound`
   without acquiring the lock. The loom harness in
   `kernel/ipc/tests/loom.rs` model-checks this state machine
   exhaustively.
2. **Capability check** against the sender's `TaskCapabilities`.
3. **Size check** against the per-port `max_payload`, additionally
   bounded by the global ABI cap `IPC_MESSAGE_MAX_PAYLOAD_LEN` (1 MiB
   in `abi-v1`). Oversize sends return `Errno::MessageTooLarge`
   (semantically `EMSGSIZE`).
4. **Capacity check** against the bounded mailbox.

Every rejection emits exactly one audit event before returning.
`Port::destroy` drains in-flight messages and is idempotent.

## Named-port registry

A `Port` on its own is anonymous: `Port::create` proves bind authority
and returns an owned value, but a sender or receiver still needs to
reach it by the `EndpointId` carried in an `IpcMessageHeader`.
`PortRegistry` (`kernel/ipc/src/registry.rs`) is that map from
`EndpointId` to the live kernel-owned `Port`.

Like `kernel/sec`'s `CapTable`, the registry has **no interior
mutability**: it exposes a plain `&self` / `&mut self` surface and owns
no lock. `kernel/core`'s `KernelState` composes it with the scheduler
and capability table under one lock-ordering policy (`AGENTS.md` §2.1 —
no global mutable static). Lookups borrow `&self` so concurrent senders
share a read guard while each `Port::send` re-checks the per-send
capability.

* `register(port)` binds the port under its own `id`, fail-closed: a
  duplicate `EndpointId` is refused with `Errno::AlreadyExists`
  (`PORT_REGISTER_DENIED`) and the supplied port is handed back boxed so
  the caller can tear it down — a live binding is never overwritten.
  Success emits `PORT_REGISTERED`.
* `lookup(id)` / `contains(id)` resolve a binding; a miss is `None`
  (mapped to `Errno::NotFound` at the syscall boundary) and is not a
  security decision, so it is not audited.
* `unregister(id)` removes and `Port::destroy`s the binding (draining
  in-flight messages, emitting `PORT_DESTROYED`) and then emits
  `PORT_UNREGISTERED`; an unknown endpoint is `Errno::NotFound`.

The registry performs no capability check of its own — bind authority
was proven at `Port::create` time and send authority is re-checked on
every `Port::send` — so it is a pure ownership map. Wiring it into the
`ipc_send` / `ipc_recv` syscall handlers awaits the user-memory copy-in
path (the same prerequisite `cap_delegate` is waiting on).

## Shared memory

`SharedMemory::create` allocates a kernel-tracked, zero-on-free
`SensitiveBuffer` (so any credential or capability-token bytes that
ever transited the region are wiped on revocation). `SharedMemory::map`
gates each mapping on the recipient's capabilities; the returned
`ShmemMapping` holds the buffer alive through an `Arc<Inner>` and
exposes `with_bytes` / `with_bytes_mut` accessors that yield `None`
once `SharedMemory::revoke` has run. Revocation thus invalidates every
live mapping atomically and the racing-mapper integration test in
`kernel/ipc/tests/integration.rs` confirms there is no torn-buffer
window between the two.

## Notifications

`NotificationChannel` is a lossless OR-accumulating signal: senders
raise one or more bits with `signal(flags)` and the bound receiver
takes-and-clears the pending set with `take_pending()` (one atomic
swap). The same bind-/send-time capability split as ports applies.

## Audit catalogue

Audit events live in the `kernel/ipc` reserved range `3_000..4_000`
(see `lib/log` event-id conventions; `kernel/sec` owns `1_000..2_000`,
`kernel/mem` owns `2_000..3_000`).

| ID   | Level | Name                          | When |
|-----:|-------|-------------------------------|------|
| 3000 | Info  | `PORT_CREATED`                | A capability-checked port was created. |
| 3001 | Error | `PORT_CREATE_DENIED`          | A port-creation request was refused. |
| 3002 | Info  | `PORT_DESTROYED`              | A port was destroyed. |
| 3003 | Info  | `PORT_REGISTERED`             | A port was bound into the named-port registry. |
| 3004 | Error | `PORT_REGISTER_DENIED`        | A registration was refused (the `EndpointId` was already bound). |
| 3005 | Info  | `PORT_UNREGISTERED`           | A port was removed from the registry and destroyed. |
| 3010 | Info  | `MESSAGE_DELIVERED`           | A message was enqueued for delivery. |
| 3011 | Error | `MESSAGE_SEND_DENIED`         | Sender lacks the port's required capabilities. |
| 3012 | Error | `MESSAGE_TOO_LARGE`           | Payload exceeded `max_payload`. |
| 3013 | Error | `MESSAGE_SEND_TO_CLOSED_PORT` | A send raced with destruction and lost. |
| 3014 | Error | `MAILBOX_FULL`                | The receiver's mailbox was full. |
| 3020 | Info  | `SHMEM_CREATED`               | A shared-memory object was created. |
| 3021 | Info  | `SHMEM_MAPPED`                | A mapping was established. |
| 3022 | Error | `SHMEM_MAP_DENIED`            | A mapping request was refused. |
| 3023 | Info  | `SHMEM_REVOKED`               | A shared-memory object was revoked. |
| 3030 | Info  | `NOTIFY_BOUND`                | A receiver bound to a channel. |
| 3031 | Info  | `NOTIFY_SIGNALLED`            | A notification was delivered. |
| 3032 | Error | `NOTIFY_SIGNAL_DENIED`        | Sender lacks the channel's signal capabilities. |

Adding a new event requires assigning the next free identifier in
`kernel/ipc/src/audit.rs` and appending a row to this table.

## Error mapping

| Errno                  | Cause                                                |
|------------------------|------------------------------------------------------|
| `PermissionDenied`     | Sender / binder / mapper lacks a required capability |
| `MessageTooLarge`      | Payload exceeded the port's `max_payload` (EMSGSIZE) |
| `LengthOutOfRange`     | Configuration out of range, mailbox full             |
| `NotFound`             | Send to destroyed port, map of revoked shmem, unregister of an unbound endpoint |
| `AlreadyExists`        | Register of an already-bound `EndpointId`            |

Every error path emits a matching audit event before returning, so
"fail closed" is observable in the security trail (`AGENTS.md` §5.4).
