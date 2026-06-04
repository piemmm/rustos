//! Host tests for the `build_c_runtime` startup-vector marshalling core.
//!
//! These exercise the security-relevant half of crt0 (`plans/CCOMPAT.md`
//! CC3) without a kernel: a startup-vector block is built exactly as the
//! kernel loader will write it, handed to [`build_c_runtime`] with a scratch
//! buffer, and the resulting C `argv` / `envp` are read back and checked. The
//! per-architecture `_start` trampoline (which only carves the scratch and
//! drives `main` / `exit`) is exercised under QEMU instead.

use super::{build_c_runtime, read_total_len};
use core::ffi::c_char;

use rustos_abi::process::{ProcessStartHeader, StringSlot, PROCESS_START_MAGIC};
use rustos_abi::{Errno, ABI_VERSION_CURRENT};

/// Build a valid startup-vector block from argument and environment strings,
/// mirroring what the kernel loader will write (and `process.rs`'s own test
/// builder).
fn build_block(args: &[&[u8]], env: &[&[u8]], canary: u64) -> Vec<u8> {
    let slot_count = args.len() + env.len();
    let strings_base = ProcessStartHeader::WIRE_LEN + slot_count * StringSlot::WIRE_LEN;

    let mut slots = Vec::new();
    let mut strings = Vec::new();
    for s in args.iter().chain(env.iter()) {
        let offset = strings_base + strings.len();
        slots.push(StringSlot {
            offset: u32::try_from(offset).expect("offset fits"),
            len: u32::try_from(s.len()).expect("len fits"),
        });
        strings.extend_from_slice(s);
    }
    let total_len = strings_base + strings.len();

    let header = ProcessStartHeader {
        magic: PROCESS_START_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        arg_count: u32::try_from(args.len()).expect("argc fits"),
        env_count: u32::try_from(env.len()).expect("envc fits"),
        total_len: u64::try_from(total_len).expect("total fits"),
        canary,
    };

    let mut block = Vec::new();
    block.extend_from_slice(&header.to_le_bytes());
    for slot in &slots {
        block.extend_from_slice(&slot.to_le_bytes());
    }
    block.extend_from_slice(&strings);
    block
}

/// Read the NUL-terminated C string at `p` back into a byte vector.
///
/// # Safety
///
/// `p` must point at a NUL-terminated string in live storage (here, the
/// scratch buffer that outlives the read).
unsafe fn cstr_bytes(p: *const c_char) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = p.cast::<u8>();
    loop {
        // SAFETY: the caller guarantees a NUL terminator within the buffer.
        let byte = unsafe { *cur };
        if byte == 0 {
            return out;
        }
        out.push(byte);
        // SAFETY: still within the NUL-terminated string.
        cur = unsafe { cur.add(1) };
    }
}

/// Read pointer slot `index` of a `*const c_char` array.
///
/// # Safety
///
/// `array` must point at an array with at least `index + 1` valid slots.
unsafe fn slot(array: *const *const c_char, index: usize) -> *const c_char {
    // SAFETY: the caller guarantees the slot is in range.
    unsafe { *array.add(index) }
}

#[test]
fn lays_out_argv_and_envp_with_nul_terminators() {
    let block = build_block(
        &[b"prog", b"--flag", b"value"],
        &[b"PATH=/Apps", b"LANG=C"],
        0xDEAD_BEEF_F00D_CAFE,
    );
    let mut scratch = vec![0u8; 4096];
    let rt = build_c_runtime(&block, &mut scratch).expect("valid block");

    assert_eq!(rt.argc, 3);
    assert_eq!(rt.canary, 0xDEAD_BEEF_F00D_CAFE);

    // SAFETY: `scratch` is alive for the rest of the test and `build_c_runtime`
    // laid out NULL-terminated arrays of NUL-terminated strings within it.
    unsafe {
        assert_eq!(cstr_bytes(slot(rt.argv, 0)), b"prog");
        assert_eq!(cstr_bytes(slot(rt.argv, 1)), b"--flag");
        assert_eq!(cstr_bytes(slot(rt.argv, 2)), b"value");
        assert!(slot(rt.argv, 3).is_null());

        assert_eq!(cstr_bytes(slot(rt.envp, 0)), b"PATH=/Apps");
        assert_eq!(cstr_bytes(slot(rt.envp, 1)), b"LANG=C");
        assert!(slot(rt.envp, 2).is_null());
    }
}

#[test]
fn lays_out_an_empty_vector() {
    let block = build_block(&[], &[], 0);
    let mut scratch = vec![0u8; 64];
    let rt = build_c_runtime(&block, &mut scratch).expect("valid empty block");

    assert_eq!(rt.argc, 0);
    // SAFETY: both arrays hold exactly one (NULL) slot.
    unsafe {
        assert!(slot(rt.argv, 0).is_null());
        assert!(slot(rt.envp, 0).is_null());
    }
}

#[test]
fn preserves_empty_strings() {
    let block = build_block(&[b""], &[b""], 0);
    let mut scratch = vec![0u8; 128];
    let rt = build_c_runtime(&block, &mut scratch).expect("valid block");

    // SAFETY: as above.
    unsafe {
        assert_eq!(cstr_bytes(slot(rt.argv, 0)), b"");
        assert_eq!(cstr_bytes(slot(rt.envp, 0)), b"");
    }
}

#[test]
fn propagates_a_parse_error() {
    let mut block = build_block(&[b"x"], &[], 0);
    block[0] ^= 0xFF; // corrupt the magic
    let mut scratch = vec![0u8; 128];
    assert_eq!(build_c_runtime(&block, &mut scratch), Err(Errno::BadMagic));
}

#[test]
fn fails_closed_when_scratch_is_too_small() {
    let block = build_block(&[b"a-long-argument-string"], &[], 0);
    // Far too small to hold the pointer arrays plus the copied string.
    let mut scratch = vec![0u8; 8];
    assert_eq!(
        build_c_runtime(&block, &mut scratch),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn read_total_len_reads_the_declared_length() {
    let block = build_block(&[b"prog", b"arg"], &[b"K=V"], 0);
    let len = read_total_len(&block).expect("valid header");
    assert_eq!(len, block.len());
}

#[test]
fn read_total_len_rejects_a_bad_header() {
    assert_eq!(read_total_len(&[0u8; 8]), Err(Errno::BufferTooSmall));

    let mut block = build_block(&[b"x"], &[], 0);
    block[0] ^= 0xFF;
    assert_eq!(read_total_len(&block), Err(Errno::BadMagic));
}
