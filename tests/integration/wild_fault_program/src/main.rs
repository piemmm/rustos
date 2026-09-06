//! Ring-3 wild-fault fixture: a minimal, separately-linked pure-Rust user
//! program built once and driven in four argv-selected roles, three of which
//! take an exception the kernel must charge to the task and to nothing else
//! (`plans/OPEN-DEFECTS.md` D42, D86).
//!
//! * **`parent`** — spawns the three faulting roles below through the
//!   production `spawn` syscall and reaps each through the production
//!   blocking `wait`, asserting every one was fault-killed with exit code
//!   139 (`128 + SIGSEGV`). Its own survival is the load-bearing assertion:
//!   the parent can only reap a third child if neither of the first two
//!   parked the CPU. Exits `0` only when all three died correctly.
//! * **`jump`** — calls a function pointer built from the address of one of
//!   its own `static`s. The image builder maps data pages No-Execute, so the
//!   call takes a ring-3 **instruction-fetch** page fault: never resolvable
//!   (a file mapping is never executable), so the kernel must kill the task
//!   rather than park the core.
//! * **`ud`** — executes an architecturally-invalid opcode, raising a ring-3
//!   exception that is *not* a page fault, on a vector the CPU pushes no
//!   hardware error code for.
//! * **`gp`** — executes a privileged instruction, raising a ring-3
//!   exception on a vector the CPU *does* push an error code for (the other
//!   exception-stub shape).
//!
//! Surviving any faulting role is the defect each exists to catch, so each
//! returns a distinct non-zero code the parent surfaces.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. Off the freestanding x86_64
//! target it is an inert stub, so `cargo build --workspace`, clippy, and fmt
//! still cover the crate; the two trapping roles name mnemonics no other
//! architecture shares, and `build.rs` documents why that decision lives
//! there rather than in a target predicate here.

#![cfg_attr(wild_fault_x86_64, no_std)]
#![cfg_attr(wild_fault_x86_64, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(wild_fault_x86_64)]
mod program {
    /// Exit code the kernel records for a fault-killed task
    /// (`128 + SIGSEGV`), which the parent expects from every faulting role.
    const FAULT_EXIT_CODE: i32 = 139;

    /// A `static` in the program's own data image, used purely for its
    /// address: the image builder maps data pages No-Execute, so calling
    /// through it is an instruction fetch that cannot be satisfied.
    static NOT_CODE: u64 = 0;

    /// The `jump` role body: call into the program's own data image. The
    /// instruction-fetch fault must kill the task; returning at all is the
    /// defect this role exists to catch.
    fn jump() -> i32 {
        let target = core::ptr::from_ref(&NOT_CODE) as usize;
        // The SAFETY contract is deliberately violated: `NOT_CODE` is data,
        // mapped without execute permission, so this call must take an
        // unresolvable ring-3 instruction-fetch fault and the kernel must
        // kill the task. Nothing at `target` is ever executed.
        let not_code: extern "C" fn() = unsafe { core::mem::transmute::<usize, _>(target) };
        not_code();
        20
    }

    /// The `ud` role body: execute the always-invalid opcode (Intel SDM
    /// Vol 2B, `UD2`). The resulting ring-3 `#UD` must kill the task.
    fn invalid_opcode() -> i32 {
        // SAFETY: `ud2` is the architecturally-defined always-invalid
        // opcode. It touches no memory and raises `#UD` unconditionally,
        // which is exactly what this role provokes; the kernel's ring-3
        // terminator never returns here.
        unsafe {
            core::arch::asm!("ud2", options(nomem, nostack, preserves_flags));
        }
        21
    }

    /// The `gp` role body: execute a CPL-0-only instruction (Intel SDM
    /// Vol 2B, `HLT`). Ring 3 raises `#GP(0)` instead of halting, and the
    /// resulting exception must kill the task.
    fn privileged() -> i32 {
        // SAFETY: `hlt` is privileged, so executing it in ring 3 raises
        // `#GP` rather than halting the CPU. It touches no memory; the
        // kernel's ring-3 terminator never returns here.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
        22
    }

    /// Spawn the child registered at `path` and reap it, asserting it exited
    /// with [`FAULT_EXIT_CODE`]. Returns `0` on success or a code derived
    /// from `fail_code` naming which step failed.
    fn run_child(path: &[u8], fail_code: i32) -> i32 {
        let pid = tairix_rt::spawn(path);
        if pid <= 0 {
            return fail_code;
        }
        let mut code = 0i32;
        if tairix_rt::wait_exit(pid, &mut code) < 0 {
            return fail_code + 1;
        }
        if code != FAULT_EXIT_CODE {
            return fail_code + 2;
        }
        0
    }

    /// The `parent` role body: drive the three faulting children through the
    /// production spawn + wait path and assert each was fault-killed.
    /// Reaching the end proves no child's exception took the CPU with it.
    fn parent() -> i32 {
        let failed = run_child(b"/bin/wf-jump", 10);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/wf-ud", 13);
        if failed != 0 {
            return failed;
        }
        let failed = run_child(b"/bin/wf-gp", 16);
        if failed != 0 {
            return failed;
        }
        0
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match tairix_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"jump") => jump(),
            Some(b"ud") => invalid_opcode(),
            Some(b"gp") => privileged(),
            _ => 5,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Off the freestanding x86_64 target (`cargo build --workspace`, clippy,
// fmt) the `tairix-rt` entry path is not compiled, so this inert `main`
// keeps the crate building under the host tooling. It performs no I/O.
#[cfg(not(wild_fault_x86_64))]
fn main() {}
