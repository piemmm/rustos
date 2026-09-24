# System notices (`abi-v1`)

A **system notice** is how a process learns that a machine-wide value it
depends on has changed. It is a *state edge*: the subscriber is told only that
the value moved, and then reads the current one. Nothing queues, so nothing
overflows, nothing is dropped, and there is no history to reconcile — which is
exactly right for a value a process must *agree* with rather than witness.

The staged design, including how a topic is added, is
[`plans/NOTICE.md`](https://github.com/tairix/tairix/blob/main/plans/NOTICE.md).

## The three parts

```rust
use tairix_abi::notice::{Notice, NoticeTopic, NOTICE_PAYLOAD_MAX};
use tairix_abi::{WaitSetOp, WaitSourceKind};

// 1. Subscribe: an ordinary wait-set member whose `id` is the topic.
tairix_rt::waitset_ctl(
    set,
    WaitSetOp::Add,
    WaitSourceKind::SystemNotice,
    u64::from(NoticeTopic::Desktop.as_u32()),
    MY_TOKEN,
);

// 2. Read, when the park reports MY_TOKEN.
let mut buf = [0u8; NOTICE_PAYLOAD_MAX];
let len = tairix_rt::notice_read(NoticeTopic::Desktop, &mut buf);

// 3. Publish, if this process is the topic's authority.
tairix_rt::notice_publish(&Notice::Desktop(info));
```

No syscall subscribes: `waitset_ctl` already carries the member, so a process
that already parks on a wait-set picks the topic up with one more `Add`.

`notice_read` is unprivileged and never blocks. That matters: the wake lands on
the loop that owes the user a frame, and an IPC round trip there would be the
blocking I/O an interactive surface may not perform.

## Topics

| Topic | Payload | Published by | Read by |
|---|---|---|---|
| `Desktop` | `DesktopInfo` (28 bytes) | the holder of a seat's live display lease | every windowed application |
| `Mounts` | none — the generation *is* the news | the kernel, on every mount-table mutation | the file manager's places rail |
| `MemoryPressure` | the band depth (1 byte) | the kernel, from the pressure gauge | any process holding a reclaimable cache |

The set is closed and deliberately small. A topic exists only where a *state*
must be agreed; an occurrence a subscriber must witness individually — a
keystroke, a completed transfer — is an IPC message or its own wait source.

## Payload lengths are exact

Every topic carries a payload of one fixed length, `NoticeTopic::payload_len`.
A publish of any other length is refused rather than stored, so a subscriber
never reads a shape the publisher could not have meant. `Notice::encode` /
`Notice::decode` are the one definition both directions use.

`NOTICE_PAYLOAD_MAX` sizes a subscriber's buffer and the kernel's per-topic
retention. It is a containment bound, not a capacity: it is what stops a
topic's payload growing into a channel. It is the widest topic's own record,
the desktop's, which carries the screen, the scale, the four theme axes and the
double-click interval.

## Authority

Publishing is authorised per topic, and no topic needed a new capability:

- **`Desktop`** admits only the holder of a seat's live display lease — the one
  principal the kernel already attests owns what is on screen, and the same
  fact a `SeatInput` wait-set member and the seat-scoped reserved-endpoint bind
  are gated on. A background session is refused and re-publishes when it
  re-acquires the lease on foreground wake.
- **`Mounts`** and **`MemoryPressure`** are kernel-owned: a userland publish to
  either is refused with `PermissionDenied`.

*Reading* any topic is ungated. Each is a machine-wide fact no principal owns,
and each was already readable through an existing query — gating the read would
only force applications to guess at facts the system knows.

## Edges and generations

Readiness compares the topic's *generation* with the one the member last
observed; reporting the member ready advances that observation, so the next
wait blocks until the topic moves again. A member added while a topic already
holds an unusual value stays quiet — a subscriber reads the value once at
start-up and is then told only about moves.

Each generation comes from its topic's own source of truth:

- `MemoryPressure`'s generation is the band depth itself, so a band that
  deepens and relaxes again before the waiter runs correctly reports nothing
  to do.
- `Desktop`'s is a counter bumped only when the published record actually
  differs, so re-publishing the current value wakes nobody. A record that moves
  and moves back wakes its subscribers once with nothing changed — one read,
  no repaint — and never misses a real change.
- `Mounts`' is a counter bumped by every mount-table mutation. A refused
  mutation changed nothing and bumps nothing.

## The query/edge pairing

An application still *asks* for the desktop before it opens anything (the
window channel's `QueryDesktop`), and converges on the notice thereafter. That
is not two paths for one job: the query answers the value the application needs
before it can size anything, and the notice carries the changes — which is what
keeps a just-spawned application correct without depending on the session
having published before it started. The memory-pressure band has the same
pairing with its System Information query.

## Calling it from C

The generated header carries the topic values, each topic's exact payload
length, the ceiling, and both prototypes:

```c
#define TAIRIX_NOTICE_PAYLOAD_MAX 28u
#define TAIRIX_NOTICE_TOPIC_DESKTOP 0u
#define TAIRIX_NOTICE_PAYLOAD_LEN_DESKTOP 28u
/* ... */

uint64_t tairix_sys_notice_read(uint32_t topic, void *buf, uintptr_t len);
int32_t  tairix_sys_notice_publish(uint32_t topic, void *payload, uintptr_t len);
```

Adding a topic fails the header generator's exhaustive match until the new
topic is named, so the header cannot fall behind the enum it is generated from.
