//! Deterministic fuzz harness for the `lib/binfmt` rxe inspection view and
//! manifest summary (decoders of untrusted executable-file bytes).
//!
//! [`tairix_binfmt::rxe::RxeView::parse`] and
//! [`tairix_binfmt::rxe::ManifestSummary::parse`] decode any file a viewer
//! is pointed at. The harness invariants:
//!
//! * parsing any byte string never panics — it returns a view or a typed
//!   error (fail closed);
//! * a successful parse yields a view whose every accessor (header,
//!   segments, needed libraries, capabilities) can be walked without a
//!   panic.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates a
//! valid image/manifest built through the `lib/abi` encoders and mixes in
//! pure noise. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_abi::{
    CapabilityId, LoadHeader, ManifestHeader, NeededLibrary, RxePermission, Segment,
    ABI_VERSION_CURRENT, LOAD_FLAG_PIE, LOAD_MAGIC, MANIFEST_MAGIC, RXE_PAGE_SIZE,
};
use tairix_binfmt::rxe::{ManifestSummary, RxeView};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte string fed to the decoders.
const MAX_NOISE: usize = 1024;

/// A valid load image built through the `lib/abi` encoders.
fn valid_image() -> Vec<u8> {
    let code = Segment {
        vaddr: RXE_PAGE_SIZE,
        file_offset: 0,
        file_size: 64,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadExecute,
    };
    let data = Segment {
        vaddr: RXE_PAGE_SIZE * 2,
        file_offset: 64,
        file_size: 32,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadWrite,
    };
    let needed =
        NeededLibrary::from_reference("/System/Libraries/libexample.so").expect("valid reference");
    let header = LoadHeader {
        magic: LOAD_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: LOAD_FLAG_PIE,
        segment_count: 2,
        needed_count: 1,
        entry: RXE_PAGE_SIZE + 16,
        cfi_tag: [0xA5; 32],
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header.to_le_bytes());
    bytes.extend_from_slice(&code.to_le_bytes());
    bytes.extend_from_slice(&data.to_le_bytes());
    bytes.extend_from_slice(&needed.to_le_bytes());
    bytes
}

/// A valid manifest built through the `lib/abi` encoders.
fn valid_manifest() -> Vec<u8> {
    let header = ManifestHeader {
        magic: MANIFEST_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: 2,
        reserved0: 0,
        syscall_table_hash: [0x11; 32],
        signer_pubkey: [0x22; 32],
        signature: [0x33; 64],
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header.to_le_bytes());
    bytes.extend_from_slice(&CapabilityId::FS_MOUNT.as_u16().to_le_bytes());
    bytes.extend_from_slice(&CapabilityId::NET_RAW.as_u16().to_le_bytes());
    bytes
}

/// Decode `bytes` both ways; a success must be walkable without a panic.
fn exercise(bytes: &[u8]) {
    if let Ok(view) = RxeView::parse(bytes) {
        let _ = view.header();
        let _ = view.entry();
        let _ = view.is_pie();
        for segment in view.segments() {
            let _ = segment.permission.is_executable();
        }
        for reference in view.needed_libraries() {
            let _ = reference.len();
        }
    }
    if let Ok(summary) = ManifestSummary::parse(bytes) {
        let _ = summary.header();
        for id in summary.capabilities() {
            let _ = id.name();
        }
    }
}

#[test]
fn parse_never_panics_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "parse_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let image = valid_image();
    let manifest = valid_manifest();

    let mut iteration: u64 = 0;
    loop {
        // 1. A valid image/manifest with a handful of bytes flipped.
        let template = if rng.next_u64() & 1 == 0 {
            &image
        } else {
            &manifest
        };
        let mut mutated = template.clone();
        for _ in 0..rng.at_most(8) {
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        exercise(&mutated);

        // 2. The same, truncated or extended at random.
        let cut = rng.at_most(mutated.len());
        exercise(&mutated[..cut]);
        mutated.extend((0..rng.at_most(64)).map(|_| rng.next_u8()));
        exercise(&mutated);

        // 3. Pure noise, optionally forced to open with a real magic.
        let mut noise = vec![0u8; rng.at_most(MAX_NOISE)];
        rng.fill(&mut noise);
        if noise.len() >= 4 && rng.next_u64() & 1 == 0 {
            let magic = if rng.next_u64() & 2 == 0 {
                LOAD_MAGIC.to_le_bytes()
            } else {
                MANIFEST_MAGIC.to_le_bytes()
            };
            noise[..4].copy_from_slice(&magic);
        }
        exercise(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
