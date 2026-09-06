//! THREADS stage `T3b-u` fixture: a separately-linked pure-Rust user-mode
//! program built once and driven in six argv-selected roles.
//!
//! The consuming verticals (`tests/integration/threads_qemu_{aarch64,riscv64,
//! x86_64}`) register this one `rxe` under role-selecting argument vectors
//! (`tairix_rt::arg(1)`):
//!
//! * **`parent`** — spawns each child role below through the production `spawn`
//!   syscall, reaps it through the production blocking `wait`, and asserts the
//!   status it must carry. Exits `0` only when every child behaved.
//! * **`counter <threads> <increments>`** — spawns `threads` threads over its
//!   *one* address space, each incrementing a shared counter `increments` times
//!   under a futex `Mutex`, then joins each for its own
//!   tally. Proves the threads genuinely share one heap, that the lock is
//!   correct under contention, and that `join` observes a value — which only
//!   works because the kernel zeroes and futex-wakes the clear-on-exit word.
//! * **`rendezvous`** — waits on a `Condvar` for a
//!   worker's flag. On the verticals' single-CPU cooperative drive a *spinning*
//!   wait would starve the worker that has to set it, so completing at all
//!   proves the wait genuinely parked and the notification genuinely woke it.
//! * **`tls <threads>`** — gives each thread its own thread-local block through
//!   the thread pointer and has it read its own magic *through* the psABI
//!   register, once before and once after a syscall. Proves the register is
//!   per-thread and survives a trap and a context switch.
//! * **`exitearly`** — a thread that ends itself with `thread_exit` instead of
//!   returning a value: the joiner must be released by the kernel's clear-on-exit
//!   wake and report `Died`, never wait forever.
//! * **`parallel <workers> <rounds>`** — builds a real `lib/parallel` worker
//!   pool and runs a divided pass through it `rounds` times, comparing every
//!   round against the same pass run on one thread. Proves the fork-join
//!   protocol itself: the epoch wake, the claim, the barrier over workers, the
//!   erased dispatch pointer's lifetime, nested dispatch, and the join at drop.
//! * **`groupexit <code>`** — parks a sibling thread on a condition variable
//!   nobody will ever notify, then `exit(code)`. The process can only be reaped
//!   if the group exit drove that parked sibling to its stopping point; a fan-out
//!   that missed it leaves the process unreapable and the vertical times out.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, stack canary, panic handler, syscall wrappers, the
//! thread runtime), never the C ABI. Built position-independent and converted to
//! an `rxe` blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
extern crate alloc;

#[cfg(freestanding)]
mod program {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use tairix_parallel::JobRunner;
    use tairix_rt::sync::{Condvar, Mutex};
    use tairix_rt::thread::{Builder, JoinError, Thread};

    /// User stack each spawned thread asks for. Well under the 8 MiB default
    /// `stack-bytes` bound, so the request genuinely exercises the sized path,
    /// and ample for these bodies (the kernel demand-grows within it anyway).
    const THREAD_STACK: usize = 64 * 1024;

    /// Exit code: a role argument was missing or unparseable — a wiring defect.
    const FAIL_ARGS: i32 = 21;
    /// Exit code: `thread_create` was refused.
    const FAIL_SPAWN: i32 = 22;
    /// Exit code: a `join` did not yield the thread's value.
    const FAIL_JOIN: i32 = 23;
    /// Exit code: the shared counter did not reach the expected total, so the
    /// threads did not share one address space or the lock lost an update.
    const FAIL_COUNT: i32 = 24;
    /// Exit code: a thread's thread-pointer read did not name its own block, or
    /// two threads shared one.
    const FAIL_TLS: i32 = 25;
    /// Exit code: joining a thread that ended itself did not report `Died`.
    const FAIL_EXIT_EARLY: i32 = 26;
    /// Exit code: a child role exited with a status the parent did not expect.
    const FAIL_CHILD: i32 = 27;
    /// Exit code: `spawn` or `wait` was refused in the parent role.
    const FAIL_PARENT: i32 = 28;
    /// Exit code: an unknown or absent role — a wiring defect.
    const FAIL_ROLE: i32 = 29;
    /// Exit code: the pool was granted no worker thread, so the `parallel` role
    /// would have proved nothing about running pieces elsewhere.
    const FAIL_POOL: i32 = 30;
    /// Exit code: a divided pass did not produce what the undivided one did —
    /// a piece run twice, a piece not run, or a race between them.
    const FAIL_PARALLEL: i32 = 31;

    /// The counter every `counter` thread contends for. A single shared word
    /// behind a futex mutex: if the threads did not share one address space the
    /// total could never reach `threads * increments`.
    static COUNTER: Mutex<u64> = Mutex::new(0);

    /// The `rendezvous` role's predicate.
    static READY: Mutex<bool> = Mutex::new(false);
    /// The condition variable the `rendezvous` role's main thread parks on.
    static READY_CV: Condvar = Condvar::new();

    /// The `groupexit` role's lock and the condition variable its sibling parks
    /// on. Nothing ever notifies it: only the group exit can release that thread.
    static NEVER: Mutex<u64> = Mutex::new(0);
    /// See [`NEVER`].
    static NEVER_CV: Condvar = Condvar::new();

    /// One thread's thread-local block: a magic at offset zero, which is where
    /// the psABI thread pointer points and what [`thread_local_magic`] reads.
    #[repr(C, align(16))]
    struct TlsBlock {
        magic: u64,
    }

    /// The magic thread `index` finds through its own thread pointer. Distinct
    /// per thread and in every byte, so a shared or stale register is visible.
    const fn tls_magic(index: u64) -> u64 {
        0x5453_0000_0000_0000 ^ (index.wrapping_mul(0x0101_0101_0101_0101) | 1)
    }

    /// Read the `u64` at offset zero from this thread's psABI thread pointer.
    ///
    /// The one operation with no architecture-neutral spelling: the register is
    /// `TPIDR_EL0` on aarch64, `tp` on riscv64, and the `FS` segment base on
    /// x86_64 — which user code can address *through* but, with `CR4.FSGSBASE`
    /// off, cannot read. Reading through it is exactly what a thread-local access
    /// compiles to on each target, so this is the honest probe of the kernel's
    /// per-thread contract.
    ///
    /// # Safety
    ///
    /// The caller must have been created with a thread pointer naming a readable
    /// [`TlsBlock`] of this process.
    unsafe fn thread_local_magic() -> u64 {
        let magic: u64;
        #[cfg(threads_tp_x86_64)]
        // SAFETY: the caller guarantees the FS base names a readable `TlsBlock`,
        // whose `magic` is its first field. The read has no other effect.
        unsafe {
            core::arch::asm!(
                "mov {out}, fs:[0]",
                out = out(reg) magic,
                options(nostack, preserves_flags, readonly),
            );
        }
        #[cfg(threads_tp_aarch64)]
        // SAFETY: as above, through `TPIDR_EL0` — an unprivileged read.
        unsafe {
            core::arch::asm!(
                "mrs {tmp}, tpidr_el0",
                "ldr {out}, [{tmp}]",
                tmp = out(reg) _,
                out = out(reg) magic,
                options(nostack, preserves_flags, readonly),
            );
        }
        #[cfg(threads_tp_riscv64)]
        // SAFETY: as above, through `tp`.
        unsafe {
            core::arch::asm!(
                "ld {out}, 0(tp)",
                out = out(reg) magic,
                options(nostack, preserves_flags, readonly),
            );
        }
        magic
    }

    /// Parse a decimal `u64` argument, or [`None`] on any malformed byte (fail
    /// closed — a wiring defect must fail the role, never default).
    fn parse_u64(bytes: &[u8]) -> Option<u64> {
        let mut acc: u64 = 0;
        let mut seen = false;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
            seen = true;
        }
        seen.then_some(acc)
    }

    /// The `counter` role: `arg(2)` threads each add `arg(3)` to one shared
    /// counter under a futex mutex, and each is joined for its own tally.
    fn counter() -> i32 {
        let (Some(threads), Some(increments)) = (
            tairix_rt::arg(2).and_then(parse_u64),
            tairix_rt::arg(3).and_then(parse_u64),
        ) else {
            return FAIL_ARGS;
        };

        let mut handles = Vec::new();
        for _ in 0..threads {
            let Ok(handle) = Builder::new().stack_bytes(THREAD_STACK).spawn(move || {
                for _ in 0..increments {
                    *COUNTER.lock() += 1;
                }
                increments
            }) else {
                return FAIL_SPAWN;
            };
            handles.push(handle);
        }

        let mut joined = 0u64;
        for handle in handles {
            match handle.join() {
                Ok(tally) => joined += tally,
                Err(_) => return FAIL_JOIN,
            }
        }
        let expected = threads * increments;
        if joined != expected || *COUNTER.lock() != expected {
            return FAIL_COUNT;
        }
        0
    }

    /// The `rendezvous` role: park on a condition variable until a worker sets
    /// the predicate and notifies.
    fn rendezvous() -> i32 {
        let Ok(worker) = Builder::new().stack_bytes(THREAD_STACK).spawn(|| {
            *READY.lock() = true;
            READY_CV.notify_one();
            0u8
        }) else {
            return FAIL_SPAWN;
        };

        // Re-tested in a loop: a condition variable may wake spuriously, so the
        // predicate — never the wake — is what ends the wait.
        let mut ready = READY.lock();
        while !*ready {
            ready = READY_CV.wait(ready);
        }
        drop(ready);

        match worker.join() {
            Ok(0) => 0,
            _ => FAIL_JOIN,
        }
    }

    /// The `tls` role: give each of `arg(2)` threads its own thread-local block
    /// and have it read its own magic through the psABI thread pointer, before
    /// and after a syscall.
    fn tls() -> i32 {
        let Some(threads) = tairix_rt::arg(2).and_then(parse_u64) else {
            return FAIL_ARGS;
        };

        // Each block outlives its thread: the handles are joined before the
        // blocks are dropped at the end of this function.
        let mut blocks = Vec::new();
        let mut handles = Vec::new();
        for index in 0..threads {
            let block = Box::new(TlsBlock {
                magic: tls_magic(index),
            });
            let base = core::ptr::from_ref(block.as_ref()) as usize as u64;
            blocks.push(block);
            let Ok(handle) = Builder::new()
                .stack_bytes(THREAD_STACK)
                .thread_pointer(base)
                .spawn(move || {
                    // SAFETY: this thread was created with its thread pointer at
                    // `base`, a live `TlsBlock` the spawning thread keeps alive
                    // until after the join below.
                    let before = unsafe { thread_local_magic() };
                    // A trap and a context switch: the register must come back.
                    tairix_rt::yield_now();
                    // SAFETY: as above.
                    let after = unsafe { thread_local_magic() };
                    (before, after)
                })
            else {
                return FAIL_SPAWN;
            };
            handles.push(handle);
        }

        for (index, handle) in handles.into_iter().enumerate() {
            let Ok((before, after)) = handle.join() else {
                return FAIL_JOIN;
            };
            let expected = tls_magic(index as u64);
            if before != expected || after != expected {
                return FAIL_TLS;
            }
        }
        drop(blocks);
        0
    }

    /// The `exitearly` role: a thread that ends itself produces no value, so its
    /// joiner must be released by the kernel's clear-on-exit wake and told so.
    fn exit_early() -> i32 {
        // The default spawn: this body needs no stack size of its own, so it
        // exercises the plain entry point rather than the builder.
        let Ok(handle) = Thread::spawn(|| -> u32 { tairix_rt::thread_exit() }) else {
            return FAIL_SPAWN;
        };
        match handle.join() {
            Err(JoinError::Died) => 0,
            _ => FAIL_EXIT_EARLY,
        }
    }

    /// The `groupexit` role: park a sibling on a futex nobody will wake, then
    /// end the whole process. Reaping this process at all proves the group exit
    /// drove that parked sibling to its stopping point.
    fn group_exit() -> i32 {
        let Some(code) = tairix_rt::arg(2).and_then(parse_u64) else {
            return FAIL_ARGS;
        };
        let Ok(sibling) = Builder::new().stack_bytes(THREAD_STACK).spawn(|| -> u8 {
            // Park until the process dies. A spurious wake re-acquires the
            // mutex and parks again, so this consumes no CPU either way.
            let mut guard = NEVER.lock();
            loop {
                guard = NEVER_CV.wait(guard);
            }
        }) else {
            return FAIL_SPAWN;
        };
        // Let the sibling reach its park before the group exit, so the fan-out
        // genuinely has to wake a parked thread rather than a runnable one.
        for _ in 0..8 {
            tairix_rt::yield_now();
        }
        sibling.detach();
        // The vertical pins a small exit status in the argument, and the exit
        // ABI carries it as an `i32`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        tairix_rt::exit(code as i32)
    }

    /// The child roles the parent drives, in order. Each exits `0`; the last
    /// exits with the status the vertical pinned in `arg(2)`, which the parent
    /// reads rather than duplicating — one number, named once by the vertical.
    const CHILD_PATHS: &[&[u8]] = &[
        b"/bin/th-counter",
        b"/bin/th-rendezvous",
        b"/bin/th-tls",
        b"/bin/th-exitearly",
        b"/bin/th-parallel",
        b"/bin/th-groupexit",
    ];

    /// The `parent` role: spawn each child in turn, reap it through the
    /// production blocking `wait`, and require the status its role must carry.
    fn parent() -> i32 {
        let Some(group_exit_code) = tairix_rt::arg(2).and_then(parse_u64) else {
            return FAIL_ARGS;
        };
        // The vertical pins a small exit status in the argument, and the exit
        // ABI carries it as an `i32`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let group_exit_code = group_exit_code as i32;

        for (index, path) in CHILD_PATHS.iter().enumerate() {
            let pid = tairix_rt::spawn(path);
            if pid <= 0 {
                return FAIL_PARENT;
            }
            let mut code = 0i32;
            if tairix_rt::wait_exit(pid, &mut code) < 0 {
                return FAIL_PARENT;
            }
            let expected = if index + 1 == CHILD_PATHS.len() {
                group_exit_code
            } else {
                0
            };
            if code != expected {
                return FAIL_CHILD;
            }
        }
        0
    }

    /// One piece of the `parallel` role's pass: where its slots start in the
    /// whole buffer, and the slots themselves.
    struct Piece<'a> {
        base: u64,
        slots: &'a mut [u64],
    }

    /// The value slot `index` must end up holding. Deliberately a few dependent
    /// operations rather than one, so a piece takes long enough for the pieces to
    /// genuinely overlap in time.
    fn slot_value(index: u64) -> u64 {
        let mut acc = index ^ 0x9E37_79B9_7F4A_7C15;
        for _ in 0..64 {
            acc = acc
                .rotate_left(7)
                .wrapping_mul(0x2545_F491_4F6C_DD1D)
                .wrapping_add(index);
        }
        acc
    }

    /// Run the pass over `buffer` through `runner`, dividing it into `pieces`.
    ///
    /// Each slot is *added* to rather than assigned, so a piece run twice doubles
    /// its slots and a piece not run leaves them zero: both show up as a
    /// mismatch, which an assignment would hide.
    fn stamp(buffer: &mut [u64], runner: &dyn tairix_parallel::JobRunner, pieces: usize) {
        let per = buffer.len().div_ceil(pieces.max(1)).max(1);
        let mut split: Vec<Piece<'_>> = Vec::new();
        let mut base = 0u64;
        for slots in buffer.chunks_mut(per) {
            let len = slots.len() as u64;
            split.push(Piece { base, slots });
            base = base.wrapping_add(len);
        }
        tairix_parallel::for_each(runner, &mut split, &|piece| {
            for (offset, slot) in piece.slots.iter_mut().enumerate() {
                let index = piece.base.wrapping_add(offset as u64);
                *slot = slot.wrapping_add(slot_value(index));
            }
        });
    }

    /// The `parallel` role: prove the fork-join pool on real threads.
    fn parallel() -> i32 {
        let (Some(workers), Some(rounds)) = (
            tairix_rt::arg(2).and_then(parse_u64),
            tairix_rt::arg(3).and_then(parse_u64),
        ) else {
            return FAIL_ARGS;
        };
        /// Slots the pass covers. Large enough that a piece is real work, small
        /// enough to stay well inside the fixture's frame budget.
        const SLOTS: usize = 2048;

        let pool = tairix_parallel::Pool::with_workers(workers as usize);
        if pool.worker_count() == 0 {
            return FAIL_POOL;
        }
        let pieces = pool.width() * 3;

        // Dispatched before anything else, so the workers have had as little
        // chance to reach their loop as the runtime allows. A pool that let a
        // dispatch begin before its workers were up would wait here for an
        // acknowledgement none of them could give, and this role would hang
        // instead of failing — which is exactly how that defect showed itself.
        let mut first = alloc::vec![0u64; 64];
        stamp(&mut first, &pool, pieces);
        if first.iter().any(|slot| *slot == 0) {
            return FAIL_PARALLEL;
        }

        // The oracle: the very same pass on this thread alone.
        let mut oracle = alloc::vec![0u64; SLOTS];
        stamp(&mut oracle, &tairix_parallel::SERIAL, pieces);

        for _ in 0..rounds {
            let mut divided = alloc::vec![0u64; SLOTS];
            stamp(&mut divided, &pool, pieces);
            if divided != oracle {
                return FAIL_PARALLEL;
            }
        }

        // A dispatch issued from inside a piece must run on the calling thread
        // rather than wait for the pool it is occupying.
        let mut outer = alloc::vec![0u64; 16];
        let nested_ok = core::sync::atomic::AtomicBool::new(true);
        {
            let mut split: Vec<Piece<'_>> = outer
                .chunks_mut(4)
                .enumerate()
                .map(|(index, slots)| Piece {
                    base: index as u64,
                    slots,
                })
                .collect();
            tairix_parallel::for_each(&pool, &mut split, &|piece| {
                let mut inner = alloc::vec![0u64; 8];
                stamp(&mut inner, &pool, 4);
                if inner.iter().any(|slot| *slot == 0) {
                    nested_ok.store(false, core::sync::atomic::Ordering::Relaxed);
                }
                for slot in piece.slots.iter_mut() {
                    *slot = 1;
                }
            });
        }
        if !nested_ok.load(core::sync::atomic::Ordering::Relaxed)
            || outer.iter().any(|slot| *slot != 1)
        {
            return FAIL_PARALLEL;
        }

        // Dropping the pool stops and joins every worker. A worker that never
        // woke, or a join that never completed, hangs here and the vertical
        // reports the timeout rather than passing.
        drop(pool);
        0
    }

    /// Program entry point: dispatch on the role argument the registry row
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match tairix_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"counter") => counter(),
            Some(b"rendezvous") => rendezvous(),
            Some(b"tls") => tls(),
            Some(b"exitearly") => exit_early(),
            Some(b"parallel") => parallel(),
            Some(b"groupexit") => group_exit(),
            _ => FAIL_ROLE,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
