# FILELOCK — advisory byte-range file locking

Status: **done**. The mechanism, its userland library surface, and the
`flock` command app are live. The reference page is
`docs/src/filesystem/locking.md`; this file records the decisions a future
contributor must not re-litigate, and what is deliberately out of scope.

## Decisions

1. **One model: byte-range advisory locks owned by the open file
   description.** What Linux calls an OFD lock (`F_OFD_SETLK`). There is no
   second lock space: a whole-file lock is the range `[0, ∞)`, so this
   subsumes `flock(2)`, and POSIX record locks are **not** offered at all.
   Their owner is the process, so closing any descriptor for the file drops
   them and two threads sharing a file cannot use them — the footguns OFD
   locks were invented to remove. Do not add them back for familiarity.

2. **Advisory, never mandatory.** A lock coordinates participants who opt
   in; the access-control boundary stays the per-inode
   owner/mode/ACL/capability model. Mandatory locking would hand any
   principal with write permission a way to stall every other reader of a
   file indefinitely — authority the file's permissions never granted — and
   its write-time check is racy. Linux deleted its implementation in 5.15.
   An "opt-in enforced mode" was considered and rejected for the same
   reason. If a future request asks for enforcement, it is a design
   conversation, not a patch.

3. **Acquisition is checked even though the lock grants nothing.**
   `CAP_FS_ACCESS` at the dispatcher; the path re-resolved and
   re-authorised under the caller's attested identity on every call; a
   shared lock needs read access and an exclusive one write access; the
   per-process record count bounded by `LimitKind::FileLocks`.

4. **Keyed on `FileId`, not on a path.** A rename moves neither the lock
   nor the identity participants agree on. A backing with no stable
   identity fails closed rather than letting two unrelated nodes share one
   owner's records.

5. **Fairness is designed in, with two exemptions.** A request prepared to
   wait conflicts with earlier-queued waiters as well as holders, so a
   stream of readers cannot starve a writer. Exempt: a non-blocking request
   (it cannot starve anyone by waiting) and a conversion of a range the
   owner already holds (queueing it behind a newcomer could only deadlock
   it). Removing either exemption reintroduces a deadlock the tests pin.

6. **Deadlock detection is sound, not complete, and that is the design.**
   The wait-for graph is over descriptions, walked breadth-first with an
   explicit worklist (never recursion — a hostile process would choose the
   depth). It catches every cycle whose participants block and hold through
   the same description, which is what a program that locks the file it has
   open does, and every self-deadlock. It does not catch a cycle whose
   participants block through one description while holding through
   another; that wait ends when a participant closes or exits and stays
   interruptible throughout, so it is recoverable rather than an unkillable
   hang. Linux detects none of these cases for OFD locks. Closing the
   remaining case exactly needs an AND-OR fixpoint over
   description→process→task reachability, which would couple the lock
   registry to the address-space registry and the thread table; that
   coupling is the reason it was not done, not an oversight.

7. **Release is the kernel's job.** The owner identity lives on the open
   file description (`kernel/core/src/aspace.rs`, `Description`), whose
   `Drop` releases the locks. Close, exit, kill and fault all go through
   it, so there is no stale-lock problem and no unlock a program can forget.

8. **Two projections, not a coverage partition.** Conflict detection reads
   an ordered map of the file's exclusive records (non-overlapping by the
   meaning of exclusivity, so an exact `O(log n)` interval index) plus a
   count of the shared ones. A full coverage partition carrying per-run
   owner sets was considered: it would make the exclusive-versus-many-
   shared-owners case `O(log n)` too, at the cost of a second accumulating
   invariant that can drift from the records it describes. Both projections
   here are *derived* from the records on every mutation, so drift is not
   representable. Do not replace them with an incrementally-nudged index
   without a property test that rebuilds and compares.

## Shape

| Piece | Path |
|---|---|
| ABI vocabulary | `lib/abi/src/filelock.rs` (`LockMode`, `LockFlags`, `LockRange`, `LockConflict`) |
| Syscalls | `FS_LOCK` (120), `FS_LOCK_QUERY` (121) in `lib/abi/src/syscall.rs` + `syscalls.rs` |
| Manager | `kernel/core/src/filelock.rs`, tests in `filelock_tests.rs` |
| Owner identity | `Description` in `kernel/core/src/aspace.rs` |
| Wait queue | `FILE_LOCK_WAITQ` + `file_lock_wake` in `kernel/core/src/waitq.rs` |
| Handlers | `fs_lock` / `fs_lock_query` in `kernel/core/src/syscalls.rs` |
| Resource limit | `LimitKind::FileLocks`, default `default_file_lock_records` in `kernel/core/src/rlimit.rs` |
| Userland library | `tairix_rt::File::{lock, try_lock, lock_timeout, unlock, lock_conflict}` |
| Command app | `userland/apps/flock` |
| Reference | `docs/src/filesystem/locking.md` |

`deadline_for` moved from `kernel/core/src/futex.rs` to
`kernel/core/src/waitq.rs` when the lock wait became its second consumer:
it is the one definition every timed park site reads.

## Remaining

* **A QEMU vertical.** The manager, the handlers and the descriptor
  lifecycle are host-tested, including two randomised runs against a
  per-byte model and a per-owner exclusion model. What no host test shows
  is the whole path on a running kernel: two hardware-isolated user tasks
  contending, one parking on `FILE_LOCK_WAITQ` and being woken by the
  other's release, and a task's locks releasing when it exits rather than
  unlocking. `tests/integration/fp_isolation_qemu_riscv64` is the template
  (two address spaces, two user kthreads, one rxe fixture each).
* **`vim`'s consumer.** `plans/VIM.md` V6 names advisory locking as a
  prerequisite for its swap-file and file-change detection work. The seam
  it wanted now exists.
