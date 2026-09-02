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

## Payload confidentiality

Every kernel-owned payload copy — a `Port` message, a `CallEndpoint`
request, a `CallEndpoint` reply — is a
[`SensitiveBuffer`](./memory.md#4-sensitive-region-api) and is therefore
zeroed the moment the kernel releases it, whichever way the transfer ends:
delivered and drained, refused for capability, size, or capacity, withdrawn
by its poster, reaped on a deadline, or cancelled by teardown. The staging
buffers the `ipc_send` / `ipc_call` / `call_post` / `call_reply` handlers
copy user bytes into are the same type, so no stage of the path holds
plaintext in a plain allocation.

This is unconditional rather than a per-endpoint property. The kernel heap
is shared across every principal and `lib/kalloc` does not zero on free, so
an un-wiped release leaves the bytes readable by whatever allocates the
block next; and the endpoints that carry a secret are not knowable at bind
time — the session and elevation exchanges carry a passphrase, the app-data
vault carries an application's sealed secrets, and a delegation carries a
capability token. An opt-in "this endpoint carries secrets" bit would be
open-by-default for every endpoint whose author did not anticipate one.

The cost is one write pass over a payload the path already copies at least
twice, and the copy is taken outside the endpoint's spinlock so the wipe is
never paid inside a critical section. Both `post` and `reply` therefore
report `Errno::OutOfMemory` if the kernel-owned copy cannot be allocated,
rather than aborting on a failed `Vec` growth.

`kernel/ipc/src/payload_wipe_tests.rs` holds the regression cover: a
test-only global allocator scans every released block for the payload
sentinel, and one of its cases leaks a payload deliberately so the scan
cannot pass vacuously.

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
check of its own; the **syscall layer** gates every receive — the
`ipc_recv` handler checks the caller against the port's
`required_recv_caps` *before* any message is observed or dequeued (the
same handler-side receive gate `call_recv` applies to a call endpoint),
refusing an under-capable caller with `Errno::PermissionDenied` while the
message stays queued and nothing about the mailbox — not even whether it
is empty — is revealed. Bind-time proof alone is not enough: the binder
holding the capabilities says nothing about who later names the endpoint
id in a receive. The `ipc_recv` syscall is built on this pair of checks
(see the syscall page).

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
([`tairix_kernel_mem::copy_in`](./memory.md#3a-user-memory-copy-uaccess))
and hands it to `Port::send`, returning `Errno::BadAddress` (the TAIRiX
`EFAULT`) for a faulting buffer or a caller with no registered address
space. A bound `ipc_recv` is now wired too: it copies the head message
out through the kernel's `copy_to_user` boundary
([`tairix_kernel_mem::copy_out`](./memory.md#3a-user-memory-copy-uaccess))
using `Port::recv_with` (below), returning `Errno::WouldBlock` (the
TAIRiX `EAGAIN`) when the bound mailbox is momentarily empty and
`Errno::BadAddress` for a faulting buffer. See
[the syscall handler-wiring table](./syscalls.md#handler-wiring-stage-27-follow-up-f3).

### Well-known names

A numeric `EndpointId` is an opaque handle a binder must already know.
So that a process can reach a *well-known* endpoint — a long-running
system service's rendezvous — by a stable name instead, the registry
keeps a second index from `PortName` (`lib/abi`, §9) to `EndpointId`.
(Desktop input is deliberately **not** a named port: a port's receive
gate is capability-only, so it cannot express "only the live seat-lease
holder may drain"; the pointer/keyboard streams flow through the seat
registry's owner-gated channels instead — see
[the seat page](../desktop/seat.md).)

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
*outstanding* calls (`AGENTS.md` §24.4), not a scaling capacity. Because it
reaches `create` straight from the endpoint-create syscall's argument, it is
itself bounded by `IPC_CALL_CAPACITY_MAX`: an unbounded value would leave the
endpoint with no memory bound at all.

The three call states are held in flat, doubling collections bounded by that
capacity, and lookup by ticket is a linear scan over them. No call operation
allocates or frees a per-call node, and every removed payload is moved out and
dropped *after* the endpoint lock is released, so the receive and reply paths
never descend into the kernel heap's own global, IRQ-masking lock while
holding this endpoint's lock.

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

A caller's calls die with it. When a task exits (cleanly, by fault, or
killed — e.g. a class driver unloaded by `devmgr`), the kernel's task
reclamation cancels every call that task still has in flight on every
registered endpoint (`cancel_posted_by`): queued requests are dropped
before any server receives them, in-service tickets are retired so the
server's later `reply` is refused fail-closed, and unclaimed replies are
discarded. Without this a dead caller's queued request would be handed to
the server as if live — serviced on a ticket whose reply goes nowhere and,
on a single-slot protocol such as the USB URB transport, wedging the
endpoint against the caller's replacement (the observed Pi 4
keyboard-recovery defect). Each affected endpoint records one
`CALL_POSTER_VANISHED` (3051) event.

Because a queued call can vanish this way, a wait-set's endpoint
readiness peek is a hint, not a guarantee. The `call_recv` syscall
therefore takes a `CallRecvFlags` word: `0` blocks until a request
arrives (the dedicated-server mode `sysinfod`/`journald`/`seatmgr` use),
while `NON_BLOCKING` answers an empty queue with `WouldBlock` instead of
parking — the mode every wait-set-driven event loop (the USB HCD, the
display service, `usb_msd`, login's elevation broker) uses so a loop
serving several sources can never park on one of them. Reserved flag
bits are rejected fail-closed.

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
| 3010 | Debug | `MESSAGE_DELIVERED`           | A message was enqueued for delivery. |
| 3011 | Error | `MESSAGE_SEND_DENIED`         | Sender lacks the port's required capabilities. |
| 3012 | Error | `MESSAGE_TOO_LARGE`           | Payload exceeded `max_payload`. |
| 3013 | Error | `MESSAGE_SEND_TO_CLOSED_PORT` | A send raced with destruction and lost. |
| 3014 | Debug | `MAILBOX_FULL`                | The receiver's mailbox was full. |
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
| 3047 | Debug | `CALL_QUEUE_FULL`             | The endpoint's outstanding-call queue was full. |
| 3048 | Debug | `CALL_REPLIED`                | A server delivered a reply to an in-flight call. `Debug` for the same reason as `CALL_POSTED` (3043): routine high-throughput RPC completion. Its denial (3049) stays at `Error`. |
| 3049 | Error | `CALL_REPLY_DENIED`           | A reply was refused. `reason` discriminates: `oversize_reply` (exceeded `max_reply`) or `unknown_ticket` (no such in-flight call — timed out, cancelled, or forged). A late reply after a missed deadline is preceded by its own `CALL_TIMED_OUT` (3053). |
| 3053 | Warn  | `CALL_TIMED_OUT`              | An in-flight call's deadline elapsed before the server replied; the ticket is retired and a late reply is refused. Recorded at the IPC layer because an in-kernel caller (the filesystem's block path) never reaches the syscall dispatcher's audit — without it, a device that stopped answering leaves no trace but the puzzling refusal of its own late reply. Edge-triggered: retiring the ticket means a later poll finds nothing. |
| 3050 | Error | `CALL_ENDPOINT_REGISTER_DENIED` | A registry bind was refused because the `EndpointId` was already bound; the freshly created endpoint is dropped (mirrors `PORT_REGISTER_DENIED`, 3004). |
| 3051 | Info  | `CALL_POSTER_VANISHED`        | A caller task exited with calls still in flight on this endpoint; the kernel cancelled them (queued requests dropped before service, in-service tickets retired so the server's reply fails closed, unclaimed replies discarded). |
| 3052 | Info  | `CALL_ENDPOINT_GRANTS_REVOKED` | A destroyed endpoint's delegated per-endpoint grants were revoked. |
| 3060 | Error | `PAYLOAD_ALLOC_FAILED`        | The kernel heap could not hold the wiped-on-drop copy of a port send, call post, or reply, so the transfer failed closed with `Errno::OutOfMemory`. Machine distress rather than back-pressure, hence `Error` where `MAILBOX_FULL` / `CALL_QUEUE_FULL` are `Debug`. |

Adding a new event requires assigning the next free identifier in
`kernel/ipc/src/audit.rs` and appending a row to this table.

## Error mapping

| Errno                  | Cause                                                |
|------------------------|------------------------------------------------------|
| `PermissionDenied`     | Sender / binder / mapper lacks a required capability |
| `MessageTooLarge`      | Payload exceeded the port's `max_payload` (EMSGSIZE) |
| `LengthOutOfRange`     | Configuration out of range                           |
| `WouldBlock`           | Receiver's mailbox full / endpoint's call queue full — transient back-pressure, retry |
| `NotFound`             | Send to destroyed port, map of revoked shmem, unregister of an unbound endpoint, publish naming an unregistered endpoint, withdraw of an unbound name |
| `AlreadyExists`        | Register of an already-bound `EndpointId`, publish of an already-bound `PortName` |
| `OutOfMemory`          | The kernel-owned wiped-on-drop payload copy could not be allocated |

Every error path emits a matching audit event before returning, so
"fail closed" is observable in the security trail (`AGENTS.md` §5.4).
