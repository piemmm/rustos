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
        b"/bin/th-groupexit",
    ];

    /// The `parent` role: spawn each child in turn, reap it through the
    /// production blocking `wait`, and require the status its role must carry.
    fn parent() -> i32 {
        let Some(group_exit_code) = tairix_rt::arg(2).and_then(parse_u64) else {
            return FAIL_ARGS;
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let group_exit_code = group_exit_code as i32;

        for (index, path) in CHILD_PATHS.iter().enumerate() {
            let Ok(pid) = i32::try_from(tairix_rt::spawn(path)) else {
                return FAIL_PARENT;
            };
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
