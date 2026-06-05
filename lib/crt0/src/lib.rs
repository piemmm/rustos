//! `rustos-crt0` — the C-callable `abi-v1` program startup/teardown object.
//!
//! When the `rxe` loader (`AGENTS.md` §16.5) drops into a freshly spawned
//! program it hands the program's entry trampoline a pointer to a
//! position-independent *startup-vector block* ([`rustos_abi::process`]).
//! This crate is that entry trampoline (crt0): on each native Tier-1 target
//! it provides the program's `_start` symbol, sets up the C runtime
//! environment (stack alignment per the platform C ABI, the `argc` / `argv` /
//! `envp` a C `main` expects), installs the per-process stack canary, calls
//! the program entry point, and routes its return value through the
//! `exit` syscall (`rustos_abi_sys::sys_exit`, the `ros_sys_exit` stub).
//!
//! Together with `rustos_abi_sys` (the `ros_sys_<name>` syscall stubs) it
//! forms the curated `/System/Libraries/` class *System runtime / C ABI*
//! (`AGENTS.md` §16.4): the minimal libc-equivalent a program **not** written
//! in Rust links to run on RustOS. It is deliberately minimal — it starts and
//! stops the program and marshals the startup vector, nothing more — and is
//! **not** a privileged path: every capability and input check happens
//! kernel-side (`AGENTS.md` §5.4 / `plans/CCOMPAT.md` §4). See
//! `plans/CCOMPAT.md` (stage CC3).
//!
//! # Host-testable core vs. target trampoline
//!
//! The startup vector is **untrusted input** (`AGENTS.md` §19.5/§19.6), so the
//! security-relevant logic — validating the block and laying out the C
//! `argv` / `envp` — lives in the host-testable, allocation-free
//! [`build_c_runtime`]. The per-architecture `_start` assembly carve-out (the
//! `start` module, gated on a build-script-emitted `crt0_native_<arch>` cfg)
//! is the thin glue that carves scratch space from the program stack, calls
//! [`build_c_runtime`], and drives `main` / `exit`. The marshalling is
//! unit-tested on the host directly; the trampoline itself is exercised under
//! QEMU (`plans/CCOMPAT.md` CC3).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::ffi::{c_char, c_int};

use rustos_abi::process::{ProcessStart, ProcessStartHeader};
use rustos_abi::Errno;

#[cfg(any(crt0_native_x86_64, crt0_native_aarch64, crt0_native_riscv64))]
mod start;

/// The C runtime view crt0 hands the hosted program's `main`.
///
/// The pointers reference storage the caller of [`build_c_runtime`] provided
/// (the `scratch` slice — in production, a region carved from the program
/// stack); they are valid for as long as that storage lives. Both `argv` and
/// `envp` are NULL-terminated arrays of NUL-terminated C strings, exactly as
/// a hosted `main(int argc, char **argv, char **envp)` expects.
#[derive(Debug, PartialEq, Eq)]
pub struct CRuntime {
    /// Number of argument strings (`argv[0..argc]`); `argv[argc]` is NULL.
    pub argc: c_int,
    /// NULL-terminated argument vector.
    pub argv: *mut *const c_char,
    /// NULL-terminated environment vector.
    pub envp: *mut *const c_char,
    /// The per-process stack-canary seed the kernel supplied
    /// (`rustos_abi::process::ProcessStart::canary`).
    pub canary: u64,
}

/// Width, in bytes, of a machine pointer on the build target. The native
/// targets are all 64-bit; the host test build matches the host word.
const PTR_WIDTH: usize = core::mem::size_of::<usize>();

/// Round `value` up to the next multiple of [`PTR_WIDTH`], failing closed on
/// overflow.
fn align_up_ptr(value: usize) -> Result<usize, Errno> {
    let mask = PTR_WIDTH - 1;
    value
        .checked_add(mask)
        .map(|v| v & !mask)
        .ok_or(Errno::LengthOutOfRange)
}

/// Read the declared total block length from a startup-vector header.
///
/// Used by the target trampoline to size the block slice it forms from the
/// raw kernel-supplied pointer before validating it with [`build_c_runtime`].
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `header_bytes` is shorter than a header.
/// * [`Errno::BadMagic`] / [`Errno::AbiVersionUnsupported`] from
///   [`ProcessStartHeader::from_bytes`].
/// * [`Errno::LengthOutOfRange`] if `total_len` does not fit a `usize`.
pub fn read_total_len(header_bytes: &[u8]) -> Result<usize, Errno> {
    let header = ProcessStartHeader::from_bytes(header_bytes)?;
    usize::try_from(header.total_len).map_err(|_| Errno::LengthOutOfRange)
}

/// Validate the startup-vector `block` and lay out the C `argv` / `envp` a
/// hosted program expects into `scratch`, returning a [`CRuntime`] view.
///
/// `block` is treated as untrusted input: it is validated through
/// [`ProcessStart::parse`] (bounds, limits, embedded-NUL rejection) before a
/// single byte is copied. The argument and environment strings — which carry
/// no NUL terminator in the block — are copied into `scratch` and
/// NUL-terminated, and the two NULL-terminated pointer arrays are built in
/// `scratch` ahead of them. Nothing is allocated; the whole layout lives in
/// the caller-provided `scratch`.
///
/// The layout written to `scratch` is, in order: the `argv` pointer array
/// (`argc + 1` pointers), the `envp` pointer array (`envc + 1` pointers), and
/// then the NUL-terminated string bytes the arrays point at.
///
/// # Errors
///
/// * Any [`Errno`] from [`ProcessStart::parse`] if `block` is malformed.
/// * [`Errno::BufferTooSmall`] if `scratch` cannot hold the whole layout —
///   crt0 fails closed rather than truncating the runtime (`AGENTS.md` §2.9).
pub fn build_c_runtime(block: &[u8], scratch: &mut [u8]) -> Result<CRuntime, Errno> {
    let view = ProcessStart::parse(block)?;
    let argc = view.arg_count();
    let envc = view.env_count();
    let argc_us = argc as usize;
    let envc_us = envc as usize;

    // The base address `scratch` happens to sit at; the pointer arrays must
    // be machine-pointer aligned, so the first array starts at the first
    // aligned offset within `scratch`.
    let base = scratch.as_mut_ptr() as usize;
    let argv_off = align_up_ptr(base)?
        .checked_sub(base)
        .ok_or(Errno::LengthOutOfRange)?;

    let argv_slots = argc_us.checked_add(1).ok_or(Errno::LengthOutOfRange)?;
    let envp_slots = envc_us.checked_add(1).ok_or(Errno::LengthOutOfRange)?;

    let envp_off = argv_off
        .checked_add(
            argv_slots
                .checked_mul(PTR_WIDTH)
                .ok_or(Errno::LengthOutOfRange)?,
        )
        .ok_or(Errno::LengthOutOfRange)?;
    let strings_off = envp_off
        .checked_add(
            envp_slots
                .checked_mul(PTR_WIDTH)
                .ok_or(Errno::LengthOutOfRange)?,
        )
        .ok_or(Errno::LengthOutOfRange)?;

    // Copy strings and record their addresses, filling the pointer arrays as
    // we go. `cursor` tracks the next free byte of the string region.
    let mut cursor = strings_off;
    let argv_addr = base.checked_add(argv_off).ok_or(Errno::LengthOutOfRange)?;
    let envp_addr = base.checked_add(envp_off).ok_or(Errno::LengthOutOfRange)?;

    // `argc` and `envc` are each <= PROCESS_START_MAX_STRINGS (4096), so the
    // sum fits a `u32` without overflow (checked in `ProcessStart::parse`).
    for i in 0..(argc + envc) {
        let (array_off, slot, bytes) = if i < argc {
            (argv_off, i as usize, view.arg(i).ok_or(Errno::OutOfRange)?)
        } else {
            let env_index = i - argc;
            (
                envp_off,
                env_index as usize,
                view.env(env_index).ok_or(Errno::OutOfRange)?,
            )
        };
        let str_addr = base.checked_add(cursor).ok_or(Errno::LengthOutOfRange)?;

        let end = cursor
            .checked_add(bytes.len())
            .and_then(|v| v.checked_add(1))
            .ok_or(Errno::LengthOutOfRange)?;
        if end > scratch.len() {
            return Err(Errno::BufferTooSmall);
        }
        scratch[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        scratch[cursor + bytes.len()] = 0;
        cursor = end;

        write_ptr(scratch, array_off + slot * PTR_WIDTH, str_addr)?;
    }

    // NULL-terminate both arrays.
    write_ptr(scratch, argv_off + argc_us * PTR_WIDTH, 0)?;
    write_ptr(scratch, envp_off + envc_us * PTR_WIDTH, 0)?;

    Ok(CRuntime {
        argc: c_int::try_from(argc).map_err(|_| Errno::LengthOutOfRange)?,
        argv: argv_addr as *mut *const c_char,
        envp: envp_addr as *mut *const c_char,
        canary: view.canary(),
    })
}

/// Write the machine-pointer-sized `value` into `scratch` at byte `off`,
/// failing closed if it would run past the buffer.
fn write_ptr(scratch: &mut [u8], off: usize, value: usize) -> Result<(), Errno> {
    let end = off.checked_add(PTR_WIDTH).ok_or(Errno::LengthOutOfRange)?;
    if end > scratch.len() {
        return Err(Errno::BufferTooSmall);
    }
    scratch[off..end].copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

#[cfg(test)]
mod tests;
