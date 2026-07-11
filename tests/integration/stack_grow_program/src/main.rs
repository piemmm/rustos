//! `SP11c` demand-grown stack fixture: a minimal, separately-linked pure-Rust
//! EL0 program built once and driven in four argv-selected roles.
//!
//! The consuming verticals (`tests/integration/stack_grow_qemu_aarch64` /
//! `…_riscv64`; the x86_64 twin is staged, `plans/SPAWN.md` `SP11e`)
//! register this one `rxe` under role-selecting
//! argument vectors (`rustos_rt::arg(1)`) whose numeric parameters they
//! derive from the one shared stack policy (`spawn_layout`), so no policy
//! constant is ever duplicated into this program:
//!
//! * **`parent`** — spawns the three child roles below through the
//!   production `spawn` syscall and reaps each through the production
//!   blocking `wait`, asserting `grow` exits `0` and `limit` / `guard` are
//!   fault-killed with exit code 139 (`128 + SIGSEGV`). Exits `0` only when
//!   all three children behaved.
//! * **`grow <target-bytes>`** — recurses until at least `target-bytes` of
//!   stack are live (far past the eagerly committed top, so the run only
//!   completes if the fault-driven growth path backs every page), verifying
//!   on unwind that each frame's bytes survived the growth, and exits `0`.
//! * **`limit <limit-bytes> <target-bytes>`** — lowers its own `StackBytes`
//!   soft bound to `limit-bytes` through `rlimit_set` (lowering needs no
//!   capability), then recurses toward `target-bytes` (past the bound): the
//!   growth path must refuse the first past-bound page and the kernel must
//!   fault-kill the task (exit 139 observed by the parent). Surviving is a
//!   distinct non-zero exit.
//! * **`guard <reserve-bytes>`** — derives its reserved span top from the
//!   address of a stack local (startup uses well under a page, so the local
//!   rounds up to the span top) and reads the middle of the unmapped guard
//!   page `reserve-bytes` below it: the access must fault-kill the task
//!   (exit 139). Surviving is a distinct non-zero exit.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `rustos-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::{LimitKind, ResourceLimit};

    /// Page size shared by every Tier-1 MMU target this fixture runs on.
    const PAGE: u64 = 4096;

    /// Exit code the kernel records for a fault-killed task
    /// (`128 + SIGSEGV`), which the parent expects from `limit` and `guard`.
    const FAULT_EXIT_CODE: i32 = 139;

    /// Stack bytes one `burn` frame occupies (its buffer; the true frame is
    /// slightly larger with the saved registers, so recursing
    /// `target / FRAME_BYTES` frames touches *at least* `target` bytes).
    const FRAME_BYTES: usize = 2048;

    /// Parse a decimal `u64` argument, or `None` on any malformed byte
    /// (fail closed — a wiring defect must fail the role, never default).
    fn parse_u64(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() {
            return None;
        }
        let mut acc: u64 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
        }
        Some(acc)
    }

    /// Recurse `frames` levels, each touching a [`FRAME_BYTES`] buffer with
    /// per-frame sentinel bytes and re-verifying them on unwind — so a page
    /// the growth path failed to back (or backed with the wrong contents)
    /// is detected, not silently survived. Returns the number of corrupted
    /// frames (`0` when every byte survived).
    ///
    /// `#[inline(never)]` plus the volatile touches keep every frame's
    /// buffer genuinely on the stack; the post-recursion re-reads prevent a
    /// tail-call from collapsing the frames.
    #[inline(never)]
    fn burn(frames: u64, seed: u64) -> u64 {
        let mut buf = [0u8; FRAME_BYTES];
        let lo = seed as u8;
        let hi = (seed >> 8) as u8;
        // SAFETY: both writes land inside `buf`, a live local array.
        unsafe {
            core::ptr::write_volatile(buf.as_mut_ptr(), lo);
            core::ptr::write_volatile(buf.as_mut_ptr().add(FRAME_BYTES - 1), hi);
        }
        let mut bad = 0u64;
        if frames > 1 {
            bad += burn(
                frames - 1,
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1),
            );
        }
        // SAFETY: both reads land inside `buf`, still live in this frame.
        unsafe {
            if core::ptr::read_volatile(buf.as_ptr()) != lo {
                bad += 1;
            }
            if core::ptr::read_volatile(buf.as_ptr().add(FRAME_BYTES - 1)) != hi {
                bad += 1;
            }
        }
        bad
    }

    /// The `grow` role body: recurse past the committed top to at least
    /// `arg(2)` bytes of live stack and verify every frame survived.
    fn grow() -> i32 {
        let Some(target) = rustos_rt::arg(2).and_then(parse_u64) else {
            return 20;
        };
        let frames = target / FRAME_BYTES as u64;
        if frames == 0 {
            return 21;
        }
        if burn(frames, 0x5EED) != 0 {
            return 22;
        }
        0
    }

    /// The `limit` role body: lower the `StackBytes` soft bound to
    /// `arg(2)`, then recurse toward `arg(3)` bytes. The growth fault past
    /// the bound must kill the task (exit 139); every return here is a
    /// distinct failure the parent will surface.
    fn limit() -> i32 {
        let Some(limit) = rustos_rt::arg(2).and_then(parse_u64) else {
            return 30;
        };
        let Some(target) = rustos_rt::arg(3).and_then(parse_u64) else {
            return 31;
        };
        let Ok(bound) = ResourceLimit::new(limit, limit) else {
            return 32;
        };
        if rustos_rt::rlimit_set(LimitKind::StackBytes, bound) != 0 {
            return 33;
        }
        // Deliberately past the bound: the kernel must refuse the growth
        // fault and kill the task. Surviving the recursion is the defect
        // this role exists to catch.
        let _ = burn(target / FRAME_BYTES as u64, 0x5EED);
        34
    }

    /// The `guard` role body: probe the unmapped guard page below the
    /// reserved span (`arg(2)` bytes below the span top). The access must
    /// fault-kill the task (exit 139); every return here is a distinct
    /// failure the parent will surface.
    fn guard() -> i32 {
        let Some(reserve) = rustos_rt::arg(2).and_then(parse_u64) else {
            return 40;
        };
        // The startup path (`_start` → the runtime driver → this frame)
        // uses well under one page of stack, so a local's address rounded
        // up to the next page boundary is exactly the span top.
        let marker = 0u8;
        let sp = core::ptr::from_ref(&marker) as u64;
        let span_top = (sp + PAGE - 1) & !(PAGE - 1);
        let Some(span_bottom) = span_top.checked_sub(reserve) else {
            return 41;
        };
        // The middle of the guard page immediately below the span.
        let target = span_bottom - PAGE / 2;
        // SAFETY contract deliberately violated: the guard page below the
        // reserved span is never mapped, so this read must take an
        // unresolvable fault and the kernel must kill the task. Surviving
        // it is the defect this role exists to catch.
        let _ = unsafe { (target as *const u8).read_volatile() };
        42
    }

    /// Spawn the child registered at `path` and reap it, asserting it exited
    /// with `expected`. Returns `0` on success or `fail_code` on any
    /// mismatch or syscall failure.
    fn run_child(path: &[u8], expected: i32, fail_code: i32) -> i32 {
        let pid = rustos_rt::spawn(path);
        if pid <= 0 {
            return fail_code;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pid = pid as i32;
        let mut code = 0i32;
        if rustos_rt::wait_exit(pid, &mut code) < 0 {
            return fail_code + 1;
        }
        if code != expected {
            return fail_code + 2;
        }
        0
    }

    /// The `parent` role body: drive the three children through the
    /// production spawn + wait path and assert their exit codes.
    fn parent() -> i32 {
        let failed = run_child(b"/bin/sg-grow", 0, 10);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/sg-limit", FAULT_EXIT_CODE, 13);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/sg-guard", FAULT_EXIT_CODE, 16);
        if failed != 0 {
            return failed;
        }
        0
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match rustos_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"grow") => grow(),
            Some(b"limit") => limit(),
            Some(b"guard") => guard(),
            _ => 5,
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `rustos-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
