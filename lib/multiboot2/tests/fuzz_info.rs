//! Deterministic fuzz harness for the shared Multiboot2 wire layout.
//!
//! Two untrusted surfaces are exercised:
//!
//! * [`tairix_multiboot2::BootInfo::parse`] is handed whatever bytes a
//!   boot medium presents as "the Multiboot2 information structure". The
//!   invariants: parsing any byte string never panics (it returns a view
//!   or a typed error, fail closed), and a successfully-parsed structure is
//!   fully walkable — every tag iterates and every convenience accessor
//!   runs — without a panic.
//! * [`tairix_multiboot2::InfoBuilder`] assembles a structure from noisy
//!   inputs into a fixed buffer: building never panics (each append returns
//!   a value), and whatever `finish` produces must parse back cleanly and
//!   agree with what was appended (the producer and consumer never drift).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG drives
//! both surfaces. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_multiboot2::{
    BootInfo, FramebufferInfo, InfoBuilder, Mb2MemoryEntry, Mb2MemoryKind, Tag,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 40_000;

/// Largest arbitrary byte string fed to the parser / builder buffer.
const MAX_NOISE: usize = 512;

/// 8-byte-aligned scratch storage (the parser requires the structure on an
/// 8-byte boundary; the loader shell guarantees that placement).
#[repr(C, align(8))]
struct Aligned([u8; MAX_NOISE]);

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// Low 32 bits of `x`, without a narrowing `as` cast.
fn low_u32(x: u64) -> u32 {
    let b = x.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Walk every part of a parsed view; any panic here is a harness failure.
fn walk(info: &BootInfo<'_>) {
    for tag in info.tags() {
        core::hint::black_box(&tag);
    }
    if let Some(mmap) = info.memory_map() {
        for e in mmap.entries() {
            core::hint::black_box(&e);
        }
    }
    if let Some(efi) = info.efi_memory_map() {
        for d in efi.entries() {
            core::hint::black_box(d.length_bytes());
            core::hint::black_box(d.is_usable_after_exit_boot_services());
        }
    }
    core::hint::black_box(info.rsdp());
    core::hint::black_box(info.command_line());
    core::hint::black_box(info.framebuffer());
}

/// Feed arbitrary aligned bytes to the parser and walk any success.
fn fuzz_parse(next: &mut impl FnMut() -> u64, scratch: &mut Aligned) {
    let len = bounded(next(), MAX_NOISE);
    for b in &mut scratch.0[..len] {
        *b = low_byte(next() >> 13);
    }
    // Occasionally plant a plausible total_size so the parser reaches the
    // tag walk instead of tripping the header check immediately.
    if len >= 8 && next() & 1 == 0 {
        let total = u32::try_from(len).unwrap_or(u32::MAX);
        scratch.0[..4].copy_from_slice(&total.to_le_bytes());
    }
    if let Ok(info) = BootInfo::parse(&scratch.0[..len]) {
        walk(&info);
    }
}

/// Build a structure from noisy inputs and check it parses back and agrees.
fn fuzz_build(next: &mut impl FnMut() -> u64, scratch: &mut Aligned) {
    // A random usable buffer size (never below the 16-byte minimum so the
    // builder can at least construct the empty stream).
    let cap = 16 + bounded(next(), MAX_NOISE - 16);
    let buf = &mut scratch.0[..cap];
    for b in buf.iter_mut() {
        *b = 0;
    }
    let Ok(mut builder) = InfoBuilder::new(buf) else {
        return;
    };
    let mut want_basic: Option<(u32, u32)> = None;
    for _ in 0..bounded(next(), 8) {
        match bounded(next(), 4) {
            0 => {
                let lo = low_u32(next());
                let hi = low_u32(next() >> 7);
                if builder.basic_memory(lo, hi).is_ok() && want_basic.is_none() {
                    want_basic = Some((lo, hi));
                }
            }
            1 => {
                let n = bounded(next(), 6);
                let entries: Vec<Mb2MemoryEntry> = (0..n)
                    .map(|_| Mb2MemoryEntry {
                        base: next(),
                        length: next(),
                        kind: Mb2MemoryKind::from_raw(low_u32(next()) % 7),
                    })
                    .collect();
                let _ = builder.memory_map(&entries);
            }
            2 => {
                let n = bounded(next(), 48);
                let rsdp: Vec<u8> = (0..n).map(|_| low_byte(next() >> 11)).collect();
                let _ = builder.rsdp(next() & 1 == 0, &rsdp);
            }
            _ => {
                let fb = FramebufferInfo {
                    addr: next(),
                    pitch: low_u32(next()),
                    width: low_u32(next() >> 3),
                    height: low_u32(next() >> 5),
                    bpp: low_byte(next() >> 9),
                    fb_type: low_byte(next() >> 15),
                    red_field_position: 16,
                    red_mask_size: 8,
                    green_field_position: 8,
                    green_mask_size: 8,
                    blue_field_position: 0,
                    blue_mask_size: 8,
                };
                let _ = builder.framebuffer(&fb);
            }
        }
    }
    let bytes = builder.finish();
    let info = BootInfo::parse(bytes).expect("a built structure always parses");
    walk(&info);
    if let Some((lo, hi)) = want_basic {
        // The first successfully-appended basic-memory tag must survive.
        let found = info.tags().any(|tag| {
            matches!(
                tag,
                Tag::BasicMemory { lower_kib, upper_kib } if (lower_kib, upper_kib) == (lo, hi)
            )
        });
        assert!(found, "appended basic-memory tag round-trips");
    }
}

#[test]
fn info_round_trip_and_parse_never_panic() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "info_round_trip_and_parse_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut scratch = Aligned([0u8; MAX_NOISE]);
    let mut iteration: u64 = 0;
    loop {
        fuzz_parse(&mut next, &mut scratch);
        fuzz_build(&mut next, &mut scratch);
        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
