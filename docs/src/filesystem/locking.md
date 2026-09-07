# Advisory file locking

TAIRiX offers one file-locking model: **byte-range advisory locks owned by
the open file description**. It is what Linux calls an OFD lock
(`F_OFD_SETLK`) — the modern, recommended API — and it is the only model,
because there is no legacy to be compatible with.

The syscalls are `fs_lock` and `fs_lock_query`; the vocabulary is
`lib/abi/src/filelock.rs`; the manager is `kernel/core/src/filelock.rs`;
the userland surface is `tairix_rt::File::lock` and friends, plus the
`flock` command app. The staged design decisions are `plans/FILELOCK.md`.

## What a lock is, and what it is not

A lock is a **coordination** primitive between processes that agree to use
it. It confers no authority and withholds none: the access-control boundary
remains the per-inode owner/mode/ACL/capability model, and a principal that
may write a file may still write a range another owner has locked.

What the lock guarantees is that two participants who both take one never
believe they hold the same range at once.

Enforcing locks against non-participants — POSIX mandatory locking — is
deliberately absent, and not for want of effort. It would hand any
principal with write permission a way to stall every other reader of a
file indefinitely, which is authority the file's own permissions never
granted. Linux deleted its implementation in 5.15 for that reason and
because the write-time check was racy.

Acquisition is nonetheless checked, so a lock is not a way to reach past
the file's permissions:

* The syscall carries `CAP_FS_ACCESS`, the same coarse gate the
  descriptor's own open needed.
* The path is re-resolved and re-authorised under the caller's attested
  identity on **every** call — the descriptor caches no authority.
* A shared lock needs read access and an exclusive lock needs write
  access, because an exclusive lock asserts a writer's right.
* The per-process record count is bounded by the `file-locks` resource
  limit, so coordination state cannot exhaust the kernel heap.

## The owner is the open file description

A lock belongs to the open file description behind the descriptor that took
it — not to the process, and not to the descriptor number. So:

| Situation | Behaviour |
|---|---|
| Two descriptors on one description (spawn-inherited, duplicated) | Share the locks; neither conflicts with the other. |
| Two separate `fs_open`s of the same file | Two owners; they **do** conflict, even within one process or thread. |
| Last descriptor on the description closes | The locks release. |
| Process exits, by any route | Every description it held closes, so every lock releases. |

That last row is the property a lock file cannot otherwise have: there is
no stale lock to time out, because the kernel — not the program — owns the
release.

This is the model POSIX record locks got wrong. Theirs are owned by the
process, so closing *any* descriptor for the file drops them all, and two
threads coordinating through one file cannot use them at all.

## Ranges

A range is `[start, start + len)`, with `len == 0` meaning "to the end of
the address space" — so a lock taken over a growing file needs no
relocking. `LockRange::WHOLE` is the whole file.

Re-locking a range the owner already holds **converts** it, so an upgrade
(shared to exclusive), a downgrade, and a plain re-lock are one operation.
A partial conversion or a partial unlock splits one record into two, which
is why an unlock is priced against the resource limit like an acquisition:
splitting is how a process would otherwise manufacture unbounded kernel
state without locking a new byte. Releasing the *whole* of a held range
never grows the count, so a caller at its bound always has a way out.

Abutting records of the same mode merge, so extending a lock byte by byte
does not accumulate a record per byte.

## Waiting

`fs_lock` blocks by default, parks off the run queue (never a spin), and
takes an optional deadline. A wait ends in one of four ways: the lock is
granted, the deadline passes (`TimedOut`), a signal unwinds it
(`Interrupted`), or the grant would deadlock (`Deadlock`).

`LockFlags::NONBLOCK` reports `WouldBlock` instead of waiting.

Waking is targeted: a release wakes only the waiters whose blocked ranges
the freed bytes overlap, so releasing one range never disturbs a waiter
queued on a disjoint one.

### Fairness

A request conflicts with the current holders and, when it is prepared to
wait, with any **earlier-queued** waiter whose range it overlaps
incompatibly. That ordering is what stops a stream of readers starving a
writer: once the writer queues, an arriving reader that overlaps it queues
behind rather than joining the holders ahead of it. A waiter that is woken
spuriously and re-queues keeps its original place in line.

Two deliberate exemptions:

* A **non-blocking** request tests only the holders. Its contract is "tell
  me whether the lock is free", and it cannot starve anyone by waiting.
* A **conversion** — a request over a range this owner already holds —
  tests only the holders. Making it queue behind a newcomer could only
  deadlock it, since it cannot release what the newcomer waits for without
  giving up the range it is converting.

### Deadlock

A blocking request whose grant would close a cycle is refused with
`Deadlock` rather than joining a wait none of the participants could leave.
The search is an iterative breadth-first walk of the wait-for graph,
bounded by the owners it has already visited, so it neither recurses on a
depth a hostile process chooses nor expands an owner twice.

Because owners are descriptions, the graph sees a cycle whenever each
participant blocks and holds through the same description — which is what a
program that locks the file it has open does, and covers every
self-deadlock, including one between two descriptions of a single process.
A cycle whose participants block through one description while holding
through a *different* one is not detected; that wait ends when a
participant closes or exits, and it stays interruptible throughout, so it
is recoverable rather than an unkillable hang. Linux detects none of these
cases for its equivalent locks.

## Querying

`fs_lock_query` reports the first lock that would block a given request:
its mode, its range, and the process that took it. Writing zero bytes is
the answer "the request would be granted" — nothing in the way is an
answer, not an error.

The report names holders only. A queued waiter holds nothing, so reporting
one would name a lock that does not exist. It is a snapshot: it reserves
nothing, and only `fs_lock` can acquire a range.

Reporting the holder's pid is what makes the query actionable — "waiting on
pid 412" rather than "busy" — and discloses nothing the caller could not
already learn, since it must hold an authorised descriptor on the file to
ask.

## What can be locked

A lock is keyed on the file's stable `FileId` (its volume id plus the
driver's node number) rather than on a path, so a rename moves neither the
lock nor the identity two participants agree on.

Consequently a descriptor whose backing has no file to lock fails closed:

| Backing | Result |
|---|---|
| A path — regular file or directory | Lockable. |
| A pipe, a pty end, a typed resource | `NotSupported`: no file two participants could agree on. |
| A one-shot delegation (`fd_grant`) | `NotSupported`: a bounded byte access under another principal's captured identity is not a coordination role. |
| A backing reporting no stable identity | `NotSupported`, rather than letting two unrelated nodes share one owner's records. |

## Cost

Per file the authoritative state is one non-overlapping ordered record set
*per owner*. Conflict detection reads two projections of it: an ordered map
of every exclusive record on the file, and a count of the shared ones.

Exclusive records cannot overlap each other — that is what exclusivity
means — so that map is non-overlapping and an interval query over it is an
exact `O(log n)`. A shared request never reads further than it. An
exclusive request on a file with no shared records stops there too, which
is the whole cost for the record-per-row workload where every lock is
exclusive. Only an exclusive request on a file that *does* hold shared
records walks owners, and it stops at the first conflict — a walk over
participants rather than records, bounded by the resource limit that
charges for them.

Closing a file pays one relaxed atomic load while nothing in the system is
locked, so the common case is untouched.

## Resource limit

`LimitKind::FileLocks` (`ulimit` name `file-locks`) bounds the live record
count per process. The default is derived from discovered RAM — one record
per 64 KiB, with a floor so the smallest board still leaves room for real
coordination — never a hard-wired ceiling. Live usage is reported through
the System Information API like every other limit, and raising the hard
bound takes `CAP_RLIMIT_RAISE`.

## From userland

```rust
use tairix_abi::{LockMode, LockRange, OpenFlags};
use tairix_rt::File;

let file = File::open(b"/Storage/db/data.lock", OpenFlags::READ.union(OpenFlags::WRITE))?;

// Whole file, waiting as long as it takes.
file.lock(LockMode::Exclusive, LockRange::WHOLE)?;

// One record's range, or fail at once.
file.try_lock(LockMode::Shared, LockRange::new(4096, 8192)?)?;

// Who is in the way?
if let Some(conflict) = file.lock_conflict(LockMode::Exclusive, LockRange::WHOLE)? {
    // conflict.pid, conflict.mode, conflict.start, conflict.len
}
```

The handle's `Drop` closes the descriptor, which releases the locks — so
an error path does not have to unwind them.

From a shell, `flock` is the tool:

```sh
flock /Users/ian/Library/backup.lock backup-now   # wait for the lock
flock -n /Storage/db/data.lock compact            # or exit 1 at once
flock -s -w 30 /Storage/db/data.lock report       # reader's lock, 30s budget
```
