# System notices — one broadcast mechanism for machine-wide state

Status: **done**, bar the end-to-end QEMU vertical recorded at the end.

A **system notice** is the one way a process learns that a machine-wide value
it depends on has moved. It is a *state edge*, not an occurrence: a subscriber
is told only that the value changed and then reads the current one, so there is
no queue, no history, no overflow, no drop, and no hold-back. That is the right
shape for facts a process must *agree* with rather than witness — the desktop's
appearance and density, the mount table's composition, the memory-pressure
band.

Before it existed, each of those had its own arrangement and each was wrong in
its own way: the desktop was announced as a per-window `WindowEvent`, so an
application closed to its icon-bar slot was told nothing and re-opened in the
appearance the user had left; the mount table had no notification at all, so a
file manager's places rail only matched the machine if the user pressed
*Refresh*; and the memory-pressure band had a bespoke wait source whose value
then cost an IPC round trip to read, on the loop that owes the user a frame.

## Shape

Three parts, and nothing else:

| Part | What it is |
|---|---|
| `WaitSourceKind::SystemNotice` | The wait-set member. Its `id` is the topic; it is ready when the topic's generation differs from the one the member last observed, and reporting it advances that observation. No new syscall subscribes — `waitset_ctl` already carries it, so an application's existing park picks it up. |
| `notice_read(topic, buf, len)` | Unprivileged, non-blocking read of the topic's current payload. No service hop: a woken subscriber converges without blocking I/O on an interactive loop. |
| `notice_publish(topic, payload, len)` | Publish. Authorised per topic, fail-closed. Publishing the value already in force moves no generation and wakes nobody. |

The ABI is `lib/abi/src/notice.rs`: `NoticeTopic`, the `Notice` payload
encode/decode, and `NOTICE_PAYLOAD_MAX`. Every topic carries a payload of one
*exact* length (`NoticeTopic::payload_len`), so a publish of any other length is
refused rather than stored and handed to a subscriber that would then have to
defend against it. `NOTICE_PAYLOAD_MAX` is a containment bound, not a capacity:
it is what stops a topic's payload growing into a channel, so it stays fixed. A
topic that needs more than that is carrying a document, not a state edge, and
belongs on an IPC endpoint.

The kernel registry is `kernel/core/src/notice.rs` — pure data behind a
`SpinLock`, exactly as `waitset`, `callreg`, and `waitq` are. Waiters park on
the single `NOTICE_WAITQ`, whose wake is a lock-free flag drained in
dispatcher context; that is a requirement, not a preference, because the
memory-pressure topic is published from inside whatever was spending memory
when the band moved and the mount topic from a table mutation holding the
filesystem's locks.

## Topics

| Topic | Payload | Publisher | Subscribers |
|---|---|---|---|
| `Desktop` | `DesktopInfo` (12 B) | the seat's **live display lease** holder | every windowed app, through `lib/window` |
| `Mounts` | none — the generation *is* the news | the kernel, from every `MountTable` mutation | `files.app`'s places rail |
| `MemoryPressure` | the band depth (1 B) | the kernel, from `MEM_STATS` | `lib/procinfo::pressure`, for every process holding a cache |

Authority carries **no new capability**:

- `Desktop` admits only the holder of a seat's live display lease — the one
  principal the kernel already attests owns what is on screen, and the same
  fact `WaitSourceKind::SeatInput` and the seat-scoped reserved-endpoint bind
  are gated on. A background session's publish is refused; it re-publishes when
  it re-acquires the lease on foreground wake.
- `Mounts` and `MemoryPressure` are kernel-owned: a userland publish to either
  is refused outright.
- *Reading* any topic is ungated. Each is a machine-wide fact no principal
  owns and each was already readable — the desktop through the window
  channel's `QueryDesktop`, the band through the ungated System Information
  query — so gating the read would only force applications to guess.

Topics carry no seat or subject dimension. With one lease-holder per seat and
one seat in use (`SEAT_PRIMARY`), a subject field whose only value is `0` would
be speculative surface; a genuine multi-seat feature is the change that adds it.

## Generations

`notice::generation(topic)` is the one definition the readiness scan uses, and
each topic's generation comes from its own source of truth rather than a second
copy:

- `Desktop` — a counter bumped **only when the published record differs**. A
  value that moves and moves back therefore wakes its subscribers once with
  nothing changed, which costs them one read and no repaint, and never misses a
  real change. A counter cannot express "back where it was" and the record is
  too wide to be a generation itself; a hash could, but a missed wake from a
  collision would leave an application in the wrong appearance for ever, and
  that is not a trade worth making for a spurious read.
- `Mounts` — a counter bumped by every mount-table mutation. Every mutator in
  `kernel/core/src/fs/mount.rs` ends with `notice::mounts_changed()`, so no
  call site that attaches, re-backs, or removes a mount can forget to; a
  *refused* mutation changed nothing and bumps nothing.
- `MemoryPressure` — the published band's depth itself, which is what the old
  bespoke wait source used. A band that deepens and relaxes again before the
  waiter runs therefore correctly reports nothing to do.

## The query/edge pairing

`WindowClient::desktop()` remains an application's **initial** read, and the
notice carries **changes**. That is not two paths for one job: it is the same
pairing `SysinfoQueryId::MEMORY_PRESSURE` already has with the band edge, and it
is what keeps a just-spawned application correct without depending on the
session having published before it started.

## Adding a topic

1. Add the `NoticeTopic` variant and its `payload_len`, and the matching
   `Notice` variant with its encode/decode, in `lib/abi/src/notice.rs`. The
   `from_u32` walk and the exhaustive match in `tools/xtask`'s C-header
   emitter both fail to compile until the new topic is named, so the generated
   header cannot fall behind.
2. Decide where the generation comes from and add the arm to
   `notice::generation`. If the kernel already owns the value, read it from its
   source; do not store a second copy.
3. Add the publish authority. Reach for an existing kernel-attested fact — a
   lease, an ownership, a binding — before considering a capability; a new
   `CAP_*` must survive the capability-minimalism tests, and none of the three
   topics needed one.
4. Regenerate `include/` (`cargo xtask c-header --write`) and extend this
   table and `docs/src/abi/notice.md`.

A topic is justified only where a *state* must be agreed. An occurrence a
subscriber must witness individually — a keystroke, a completed transfer, a
pick conclusion — is an IPC message or a wait source, not a notice: the whole
point of the edge is that history is not kept.

## Outstanding — the end-to-end QEMU vertical

Everything the mechanism guarantees is covered by host tests: the kernel's
topic baseline, one edge per change, the revert case, the refusals (unknown
topic, wrong-length payload, a userland publish to a kernel-owned topic, a
publish without the seat lease), every mount mutator moving the generation,
`Desktop::adopt`'s accept/refuse, the session re-theming the **compositor** on
a `SetAppearance` (the regression test for the root defect — it fails before
the fix), and that a board renders differently under light and dark so an
adopted appearance demonstrably reaches pixels.

What no host test can show is the whole chain on real firmware: boot the
desktop, open an application, switch to Light from the taskbar's system menu,
and read the application's own window rectangle out of two screendumps to see
it repaint — then close the application to its icon-bar slot, switch back to
Dark, re-open it, and see it open dark. That last half is the case the
per-window announcement could not serve at all and is the reason this
mechanism exists, so it is the vertical worth having.

The machinery is all present (`tests/integration/appbar_qemu_aarch64` drives
the program library and the bar, reconstructs screen rectangles through the
production taskbar layout, and compares screendump regions), so this is a new
test crate beside it plus its enrolment rather than new infrastructure. A
two-shot comparison is required rather than an ink test: an application's
client area is filled opaque in either appearance, so "it repainted" is
"these two frames of the same rectangle differ".

## Related

`docs/src/abi/notice.md` (the ABI page), `plans/APPS.md` (the obligation on a
GUI application to adopt and follow the desktop's appearance), `plans/DISPLAY.md`
(the seat and its display lease), `plans/SMARTRAM.md` (the pressure band),
`plans/NEW-FILEMANAGER.md` (the places rail), `plans/APPWIN.md` (the app-window
channel the desktop topic replaced an event on).
