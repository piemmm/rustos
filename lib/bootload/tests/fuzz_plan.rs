//! Deterministic fuzz harness for the `lib/bootload` loader core (a
//! validator of untrusted kernel-image bytes).
//!
//! [`tairix_bootload::plan_kernel_load`] is handed whatever bytes a boot
//! medium presents as "the kernel". The harness invariants:
//!
//! * planning any byte string never panics — it returns a plan or a typed
//!   error (fail closed);
//! * a successful plan is internally consistent and walkable without a
//!   panic: every segment's physical end is representable, no two segments
//!   overlap, none is write-executable, and the reported span bounds every
//!   segment.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates a
//! hand-assembled valid ELF64 template and mixes in pure noise. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to
//! extend the loop to a wall-clock budget.

use tairix_binfmt::elf::Machine;
use tairix_bootload::{plan_kernel_load, LoadPlan};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte string fed to the planner.
const MAX_NOISE: usize = 1024;

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A minimal valid loadable ELF64: an `ET_EXEC` x86_64 header with two
/// non-overlapping `PT_LOAD` program headers (R+X code, R+W data).
fn valid_kernel() -> Vec<u8> {
    let phoff = 64u64;
    let mut out = Vec::new();
    out.extend_from_slice(b"\x7fELF");
    out.extend_from_slice(&[2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    push_u16(&mut out, 2); // e_type = ET_EXEC
    push_u16(&mut out, 62); // e_machine = EM_X86_64
    push_u32(&mut out, 1); // e_version
    push_u64(&mut out, 0x10_0000); // e_entry
    push_u64(&mut out, phoff);
    push_u64(&mut out, 0); // e_shoff
    push_u32(&mut out, 0); // e_flags
    push_u16(&mut out, 64); // e_ehsize
    push_u16(&mut out, 56); // e_phentsize
    push_u16(&mut out, 2); // e_phnum
    push_u16(&mut out, 64); // e_shentsize
    push_u16(&mut out, 0); // e_shnum
    push_u16(&mut out, 0); // e_shstrndx

    // PT_LOAD, R+X, at phys 0x10_0000.
    push_u32(&mut out, 1);
    push_u32(&mut out, 0b101);
    push_u64(&mut out, 0); // p_offset
    push_u64(&mut out, 0x10_0000); // p_vaddr
    push_u64(&mut out, 0x10_0000); // p_paddr
    push_u64(&mut out, 0); // p_filesz
    push_u64(&mut out, 0x1000); // p_memsz
    push_u64(&mut out, 1); // p_align

    // PT_LOAD, R+W, at phys 0x10_2000 (no overlap with the first).
    push_u32(&mut out, 1);
    push_u32(&mut out, 0b110);
    push_u64(&mut out, 0); // p_offset
    push_u64(&mut out, 0x10_2000); // p_vaddr
    push_u64(&mut out, 0x10_2000); // p_paddr
    push_u64(&mut out, 0); // p_filesz
    push_u64(&mut out, 0x1000); // p_memsz
    push_u64(&mut out, 1); // p_align

    out
}

/// A successful plan must be internally consistent.
fn check_consistent(plan: &LoadPlan) {
    let segs = plan.segments();
    assert!(!segs.is_empty(), "a plan always has at least one segment");
    let (lo, hi) = plan.phys_span().expect("a non-empty plan has a span");
    for (i, seg) in segs.iter().enumerate() {
        let end = seg
            .phys_end()
            .expect("accepted segment end is representable");
        assert!(seg.mem_size > 0, "accepted segment is non-empty");
        assert!(seg.file_size <= seg.mem_size, "file fits in memory image");
        assert!(
            !seg.flags.is_write_execute(),
            "no W^X violation is accepted"
        );
        assert!(seg.phys_dest >= lo && end <= hi, "span bounds the segment");
        for other in &segs[i + 1..] {
            let other_end = other.phys_end().expect("segment end representable");
            let overlap = seg.phys_dest < other_end && other.phys_dest < end;
            assert!(!overlap, "accepted segments never overlap");
        }
    }
}

/// Plan `bytes` for both machines; a success must be consistent.
fn exercise(bytes: &[u8]) {
    for machine in [Machine::X86_64, Machine::Aarch64, Machine::Riscv64] {
        if let Ok(plan) = plan_kernel_load(bytes, machine) {
            check_consistent(&plan);
        }
    }
}

#[test]
fn planning_never_panics_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "planning_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let template = valid_kernel();

    let mut iteration: u64 = 0;
    loop {
        // 1. The valid template with a handful of bytes flipped.
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

        // 3. Pure noise, optionally forced to open with the ELF magic.
        let mut noise = vec![0u8; rng.at_most(MAX_NOISE)];
        rng.fill(&mut noise);
        if noise.len() >= 6 && rng.next_u64() & 1 == 0 {
            noise[..6].copy_from_slice(b"\x7fELF\x02\x01");
        }
        exercise(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
