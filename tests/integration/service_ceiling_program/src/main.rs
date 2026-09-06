//! USERS `U4` service-ceiling fixture: a minimal, separately-linked
//! pure-Rust EL0 program built once and driven in two argv-selected roles.
//!
//! The consuming vertical (`tests/integration/service_ceiling_qemu_aarch64`)
//! registers this one `rxe` under role-selecting argument vectors
//! (`tairix_rt::arg(1)`); the target service uid arrives through the
//! vertical's registry argument vector (resolved from the compiled account
//! set, `tairix_users::DEVMGR_UID`), so no account constant is ever
//! duplicated into this program:
//!
//! * **`parent <uid>`** — holds `CAP_PROC_SPAWN` + `CAP_SPAWN_AS_USER` and
//!   switches the `svc` role into the devmgr service account through the
//!   production `spawn` syscall (`tairix_rt::spawn_as`), then reaps it and
//!   propagates its exit code. Exits `0` only when the switched child
//!   proved every expectation.
//! * **`svc`** — runs **as the devmgr account** under devmgr's compiled
//!   capability ceiling intersected with the vertical's deliberately
//!   over-wide manifest (devmgr's ceiling plus every sibling service's
//!   defining capability). It proves the intersection binds end to end
//!   through real traps:
//!   1. its own `CAP_SYSINFO_HW`-gated `hw_tree_read` succeeds — the
//!      ceiling keeps what the account genuinely grants;
//!   2. `spawn_as` is refused `PermissionDenied` — neither `CAP_PROC_SPAWN`
//!      nor login's `CAP_SPAWN_AS_USER` survives the intersection, so the
//!      dispatcher gate fails the identity switch closed;
//!   3. `users_db_read` is refused `PermissionDenied` — login's
//!      `CAP_USERS_READ` was stripped;
//!   4. `seat_switch` is refused `PermissionDenied` — seatmgr's
//!      `CAP_SEAT_ADMIN` was stripped;
//!   5. `sysinfo_introspect` is refused `PermissionDenied` — sysinfod's
//!      `CAP_SYSINFO_INTROSPECT` was stripped.
//!
//! It is a **pure-Rust** program: it links the Rust userland runtime
//! `tairix-rt` (`_start`, stack canary, panic handler, syscall wrappers),
//! never the C ABI. Built position-independent and converted to an `rxe`
//! blob by the consuming test's build script. On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::{Errno, IntrospectDomain, CONSOLE_INHERIT};

    /// Registry path of the `svc` role — the byte string the parent's
    /// `spawn_as` names and the `svc` role's own refused re-spawn targets.
    /// Both halves agree with the consuming vertical's registry row.
    const SVC_PATH: &[u8] = b"/bin/sc-svc";

    /// The signed `-errno` value a refused syscall surfaces for `err`.
    fn neg(err: Errno) -> i64 {
        -i64::from(err.as_i32())
    }

    /// Parse a decimal `u32` argument, or `None` on any malformed byte
    /// (fail closed — a wiring defect must fail the role, never default).
    fn parse_u32(bytes: &[u8]) -> Option<u32> {
        if bytes.is_empty() {
            return None;
        }
        let mut acc: u32 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        }
        Some(acc)
    }

    /// The `svc` role body: running as the devmgr service account, the
    /// account's own grant works while every sibling capability the lying
    /// manifest requested is refused at the audited dispatcher gate.
    fn svc() -> i32 {
        // 1. The account's own authority survives the intersection:
        //    `hw_tree_read` is `CAP_SYSINFO_HW`-gated and devmgr's ceiling
        //    carries it. The snapshot content is irrelevant; the wrapper
        //    clamps the count to the buffer.
        let mut tree = [0u8; 4096];
        if tairix_rt::hw_tree_read(&mut tree).is_err() {
            return 20;
        }
        // 2. The identity switch is refused closed: neither
        //    `CAP_PROC_SPAWN` nor login's `CAP_SPAWN_AS_USER` survived the
        //    intersection, so the dispatcher gate denies the spawn before
        //    any child state exists. uid 0 (`system`) is the most
        //    privileged identity to attempt — and it is still refused.
        if tairix_rt::spawn_as(SVC_PATH, CONSOLE_INHERIT, 0) != neg(Errno::PermissionDenied) {
            return 21;
        }
        // 3. login's `CAP_USERS_READ` was stripped: the credential
        //    database stays unreadable.
        let mut users = [0u8; 64];
        if tairix_rt::users_db_read(&mut users) != Err(neg(Errno::PermissionDenied)) {
            return 22;
        }
        // 4. seatmgr's `CAP_SEAT_ADMIN` was stripped: seat administration
        //    is refused before any seat state is touched.
        if tairix_rt::seat_switch(0, 0) != neg(Errno::PermissionDenied) {
            return 23;
        }
        // 5. sysinfod's `CAP_SYSINFO_INTROSPECT` was stripped: the
        //    privileged introspection primitive is refused.
        let mut info = [0u8; 64];
        if tairix_rt::sysinfo_introspect(IntrospectDomain::Processes.as_u32(), 0, &mut info)
            != Err(neg(Errno::PermissionDenied))
        {
            return 24;
        }
        0
    }

    /// The `parent` role body: switch the `svc` role into the service
    /// account named by the vertical (`arg(2)`, the devmgr uid) through the
    /// production `spawn` syscall, reap it, and propagate its verdict.
    fn parent() -> i32 {
        let Some(uid) = tairix_rt::arg(2).and_then(parse_u32) else {
            return 10;
        };
        let pid = tairix_rt::spawn_as(SVC_PATH, CONSOLE_INHERIT, uid);
        if pid <= 0 {
            return 11;
        }
        let mut code = 0i32;
        if tairix_rt::wait_exit(pid, &mut code) < 0 {
            return 12;
        }
        // The switched child's own verdict is the test's verdict.
        code
    }

    /// Program entry point: dispatch on the role argument the registry entry
    /// pinned (`arg(1)`). An absent or unknown role is a wiring defect and a
    /// distinct failure code (fail closed, never a default role).
    fn main() -> i32 {
        match tairix_rt::arg(1) {
            Some(b"parent") => parent(),
            Some(b"svc") => svc(),
            _ => 5,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the
// crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
