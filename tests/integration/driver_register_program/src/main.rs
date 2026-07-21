//! `PLAN.md` Stage 4.HW fixture program: a minimal, separately-linked
//! pure-Rust EL0 driver stub completing the spawned-driver register
//! handshake.
//!
//! The consuming vertical (`tests/integration/driver_spawn_qemu_aarch64`)
//! spawns this program through the production parameterised spawn path
//! (`Aarch64ProcessSpawn` image builder) with the argument vector
//! `[b"drvstub", <reply endpoint id, ASCII decimal>, <reply port name>]`
//! — exactly the shape the driver host hands a spawned driver process. The
//! stub:
//!
//! 1. reads `arg(1)` from the validated startup vector
//!    (`tairix_rt::arg`, published by `_start` before `main` runs);
//! 2. parses it as the decimal reply endpoint id;
//! 3. resolves the well-known port name handed over in `arg(2)` through
//!    the production `port_resolve` syscall and refuses to proceed unless
//!    it names the same endpoint — the publish → resolve path a process
//!    uses to find a well-known service port it was not handed directly;
//! 4. enumerates its kernel-minted device-resource grants through the
//!    `resource_grants` syscall and refuses to proceed unless exactly one
//!    well-formed register-window grant was delivered (handle 1, MMIO
//!    kind, non-zero length) — the way a user-space driver learns the
//!    windows its matched node requested;
//! 5. sends a `DriverRegisterReply::registered` record over the
//!    production `ipc_send` syscall (kernel-side capability check +
//!    copy-in);
//! 6. returns 0, which `tairix-rt` routes through the `exit` syscall.
//!
//! Each failure path returns a distinct non-zero diagnostic so the
//! vertical fails loudly: the reply never arrives
//! and the host-side budget loop reports the failure rather than a silent
//! pass.
//!
//! It is a **pure-Rust** program: it links the Rust
//! userland runtime `tairix-rt` (which provides `_start`, the stack
//! canary, the panic handler, and the `ipc_send`/`exit` syscall wrappers),
//! never the C ABI (`crt0` + `abi-sys`), which exists solely for non-Rust
//! programs. It is built position-independent and
//! converted to an `rxe` blob by the consuming test's build script. On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::hwtree::{GrantedResource, HwResourceKind};
    use tairix_abi::{DriverHandle, DriverRegisterReply};

    /// Raw value of the informational [`DriverHandle`] this stub reports.
    /// Any non-zero value works (the host mints its own unforgeable handle
    /// on success); the consuming vertical pins the same
    /// constant and asserts the decoded reply round-tripped it.
    const STUB_HANDLE_RAW: u64 = 0x00D8_0001;

    /// Parse `bytes` as a non-negative decimal `u64`, or `None` on an
    /// empty string, a non-digit byte, or overflow (fail closed — a malformed endpoint argument never sends).
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall. Returns 0 after a successfully sent register reply, or a
    /// distinct non-zero diagnostic per failure site.
    fn main() -> i32 {
        // The spawner placed the reply endpoint id in arg(1); an absent
        // or malformed argument is a wiring defect the vertical must
        // surface, never a silently skipped reply.
        let Some(raw) = tairix_rt::arg(1) else {
            return 10;
        };
        let Some(endpoint) = parse_u64(raw) else {
            return 11;
        };

        // The vertical also published the reply endpoint under a
        // well-known port name and handed the name over as arg(2).
        // Resolve it through the production `port_resolve` syscall
        // (copy-in, grammar validation, registry lookup) and refuse to
        // proceed unless it names the same endpoint — the name path is
        // how a process finds a well-known service port it was not
        // handed directly.
        let Some(name) = tairix_rt::arg(2) else {
            return 19;
        };
        let Ok(resolved) = u64::try_from(tairix_rt::port_resolve(name)) else {
            return 20;
        };
        if resolved != endpoint {
            return 21;
        }

        // Verify the spawn minted and delivered this driver's device-
        // resource grant before replying: a user-space driver reaches its
        // windows only through the grants `resource_grants` enumerates. Exactly one well-formed register-window
        // grant must arrive (handle 1, MMIO kind, non-zero length); any
        // shortfall is a wiring defect the vertical must surface, never a
        // silently sent reply.
        let mut grant_buf = [0u8; GrantedResource::WIRE_LEN];
        let read = tairix_rt::resource_grants(&mut grant_buf);
        if read != GrantedResource::WIRE_LEN as i64 {
            return 14;
        }
        let Ok(grant) = GrantedResource::from_bytes(&grant_buf) else {
            return 15;
        };
        if grant.handle != 1 {
            return 16;
        }
        if grant.resource.kind() != Some(HwResourceKind::Mmio) {
            return 17;
        }
        if grant.resource.length() == 0 {
            return 18;
        }

        // Build the informational success reply. `from_raw` refuses only
        // the zero sentinel, which `STUB_HANDLE_RAW` is not; surface a
        // distinct diagnostic anyway rather than unwrapping.
        let Ok(handle) = DriverHandle::from_raw(STUB_HANDLE_RAW) else {
            return 12;
        };
        let reply = DriverRegisterReply::registered(handle);

        // The production `ipc_send` path: kernel-side endpoint resolution,
        // payload bound, copy-in from this process's own address space,
        // and the per-send capability check. A
        // negative return is `-errno`.
        if tairix_rt::ipc_send(endpoint, &reply.to_le_bytes()) < 0 {
            return 13;
        }
        0
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
