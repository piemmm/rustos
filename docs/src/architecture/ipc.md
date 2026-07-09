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

The receive side offers two shapes. `Port::recv` pops and returns the
oldest message unconditionally. `Port::recv_with(f)` is a **peek/commit**
variant for receivers that must move the payload across a fallible
boundary before they can accept it: it holds the mailbox lock while it
runs `f` against the head message and dequeues it **only** when `f`
returns `Ok`, so a failure — for example a faulting `copy_to_user` when
the kernel delivers the payload into the receiver's address space, or a
receive buffer too small to hold it — leaves the message at the head of
the mailbox to be re-delivered rather than dropping it on the floor
(`AGENTS.md` §5.4, fail closed). Like `recv` it performs no capability
check; the receiver's authority was fixed at bind time. The `ipc_recv`
syscall is built on this primitive (see the syscall page).

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
every `Port::send` — so it is a pure ownership map. It is now composed
into `kernel/core`'s `KernelState` as `ipc: RwLock<PortRegistry>`
(mirroring `caps: RwLock<CapTable>`) and borrowed by the
`KernelDispatchHook`, so the `ipc_send` / `ipc_recv` handlers resolve
the syscall's endpoint against the live map: an unbound endpoint fails
closed with `Errno::NotFound`. A bound `ipc_send` is fully wired — it
copies the payload in through the kernel's `copy_from_user` boundary
([`rustos_kernel_mem::copy_in`](./memory.md#3a-user-memory-copy-uaccess))
and hands it to `Port::send`, returning `Errno::BadAddress` (the RustOS
`EFAULT`) for a faulting buffer or a caller with no registered address
space. A bound `ipc_recv` is now wired too: it copies the head message
out through the kernel's `copy_to_user` boundary
([`rustos_kernel_mem::copy_out`](./memory.md#3a-user-memory-copy-uaccess))
using `Port::recv_with` (below), returning `Errno::WouldBlock` (the
RustOS `EAGAIN`) when the bound mailbox is momentarily empty and
`Errno::BadAddress` for a faulting buffer. See
[the syscall handler-wiring table](./syscalls.md#handler-wiring-stage-27-follow-up-f3).

### Well-known names

A numeric `EndpointId` is an opaque handle a binder must already know.
So that a process can reach a *well-known* endpoint — the desktop's
pointer- and keyboard-input ports, a long-running system service — by a
stable name instead, the registry keeps a second index from `PortName`
(`lib/abi`, §9) to `EndpointId`:

* `publish_name(name, id)` binds a validated `PortName` to a
  currently-registered endpoint. It fails closed: a name already in use
  is `Errno::AlreadyExists` and an endpoint that is not registered is
  `Errno::NotFound` (both `PORT_NAME_PUBLISH_DENIED`), so a name can
  never resolve to a non-existent port and a live name is never silently
  re-pointed. Success emits `PORT_NAME_PUBLISHED`.
* `resolve(name)` / `resolve_port(name)` map a name back to its
  `EndpointId` (or directly to the live `Port`); a miss is `None` and is
  not audited, mirroring `lookup`.
* `withdraw_name(name)` removes a single name binding, leaving the
  endpoint registered, and emits `PORT_NAME_WITHDRAWN`; an unbound name
  is `Errno::NotFound`.

The index only ever points at a live binding: `unregister(id)` withdraws
every name that resolved to `id` (one `PORT_NAME_WITHDRAWN` each) before
destroying the port, so a resolution can never dangle. A name grants no
authority of its own; the per-send capability check is unchanged.

User space reaches the index through the `port_resolve` syscall
(`SyscallNumber::PORT_RESOLVE`): the kernel bounds the supplied length
against `PORT_NAME_MAX_LEN` before touching user memory, copies the name
bytes in through the validated `copy_from_user` boundary, validates them
with `PortName::from_ascii`, and resolves them against the live registry
— returning the bound `EndpointId` for `ipc_send` / `ipc_recv`, or
`Errno::NotFound` for an unpublished name. Like the other pure observers
(`cap_query`, `clock_get`) it is unprivileged and unaudited: resolving a
name grants nothing, publication is a kernel-side bind-authority-checked
operation, and every send to the resolved endpoint is still
capability-checked at the port. The aarch64 driver-spawn QEMU vertical
exercises the whole path live: the test kernel publishes its reply
endpoint under a well-known name and the spawned stub resolves it over
the syscall before replying.

A `PortName` is a non-empty, ≤ 31-byte ASCII string that starts with a
lowercase letter and continues with lowercase letters, digits, `'.'`, or
`'_'`, with no trailing `'.'` and no `".."`. The constrained alphabet
keeps a name canonical, log-printable, and free of separators a routing
layer might re-interpret; its `from_ascii` / `from_bytes` decoders reject
anything else and are exercised by the `lib/abi` fuzz harness (§19.6).

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

## Synchronous call/reply endpoints

A `Port` is fire-and-forget; the reactive driver-store file service
(Design D — `/System` file-read IPC service) and any future request/reply
system service need *synchronous* semantics instead. `CallEndpoint` is
that primitive: a caller `post`s a request and receives an opaque,
unforgeable `CallTicket`; the single bound server drains the oldest
request with `recv_call` (moving it to an in-service table keyed by its
ticket) and answers with `reply(ticket, …)`; the caller claims the answer
with `take_reply(claimant, ticket)`. The bind-/send-time capability split
and size/closed-port checks mirror `Port` exactly; `create` takes its
bounds as one `CallEndpointLimits` value (`max_request`, `max_reply`,
`capacity`), where `capacity` is a fail-closed bound on the number of
*outstanding* calls (`AGENTS.md` §24.4), not a scaling capacity.

`CallEndpoint` is the request/reply *state machine* only and never
blocks — the caller parking until its ticket is replied and the server
parking until a request arrives are layered above through the same
cooperative yield/park seam the IRQ-wait and `wait` syscalls use
(`kernel/core`), so the primitive stays scheduler-free and host-testable
(`AGENTS.md` §2.2 / §17.4). Two security properties beyond `Port`:
`take_reply` takes the claiming task id and a reply is claimable only by
the task that posted it (a mismatch is reported as `Unknown`, never
revealing another task's ticket — `AGENTS.md` §19.1); and `destroy`
cancels every in-flight ticket so a parked caller observes `Cancelled`
and abandons rather than waiting forever (fail closed, `AGENTS.md` §2.9).

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
| 3006 | Info  | `PORT_NAME_PUBLISHED`         | A well-known name was bound to an endpoint. |
| 3007 | Error | `PORT_NAME_PUBLISH_DENIED`    | A name binding was refused (name already bound, or its endpoint is not registered). |
| 3008 | Info  | `PORT_NAME_WITHDRAWN`         | A well-known name binding was removed (explicitly, or because its endpoint was unregistered). |
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
| 3040 | Info  | `CALL_ENDPOINT_CREATED`       | A capability-checked synchronous call endpoint was created. |
| 3041 | Error | `CALL_ENDPOINT_CREATE_DENIED` | A call-endpoint creation request was refused. |
| 3042 | Info  | `CALL_ENDPOINT_DESTROYED`     | A call endpoint was destroyed (in-flight callers fail closed). |
| 3043 | Debug | `CALL_POSTED`                 | A request was posted to a call endpoint, awaiting a reply. `Debug`, not `Info`: the synchronous call path is the high-throughput RPC transport (e.g. the USB URB endpoint), so a successful post is routine throughput that would flood the log two records per round-trip; it stays available when the level is lowered for forensics. Its denials (3044–3047) stay at `Error`. |
| 3044 | Error | `CALL_POST_DENIED`            | Caller lacks the endpoint's required capabilities. |
| 3045 | Error | `CALL_REQUEST_TOO_LARGE`      | Request payload exceeded `max_request`. |
| 3046 | Error | `CALL_POST_TO_CLOSED_ENDPOINT`| A post raced with destruction and lost. |
| 3047 | Error | `CALL_QUEUE_FULL`             | The endpoint's outstanding-call queue was full. |
| 3048 | Debug | `CALL_REPLIED`                | A server delivered a reply to an in-flight call. `Debug` for the same reason as `CALL_POSTED` (3043): routine high-throughput RPC completion. Its denial (3049) stays at `Error`. |
| 3049 | Error | `CALL_REPLY_DENIED`           | Unknown ticket, or reply exceeded `max_reply`. |

Adding a new event requires assigning the next free identifier in
`kernel/ipc/src/audit.rs` and appending a row to this table.

## Error mapping

| Errno                  | Cause                                                |
|------------------------|------------------------------------------------------|
| `PermissionDenied`     | Sender / binder / mapper lacks a required capability |
| `MessageTooLarge`      | Payload exceeded the port's `max_payload` (EMSGSIZE) |
| `LengthOutOfRange`     | Configuration out of range, mailbox full             |
| `NotFound`             | Send to destroyed port, map of revoked shmem, unregister of an unbound endpoint, publish naming an unregistered endpoint, withdraw of an unbound name |
| `AlreadyExists`        | Register of an already-bound `EndpointId`, publish of an already-bound `PortName` |

Every error path emits a matching audit event before returning, so
"fail closed" is observable in the security trail (`AGENTS.md` §5.4).
