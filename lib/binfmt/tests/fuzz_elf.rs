//! Deterministic fuzz harness for the `lib/binfmt` ELF64 view (a decoder
//! of untrusted executable-file bytes).
//!
//! [`tairix_binfmt::elf::ElfView::parse`] decodes any file a viewer is
//! pointed at. The harness invariants:
//!
//! * parsing any byte string never panics — it returns a view or a typed
//!   error (fail closed);
//! * a successful parse yields a view whose every accessor (program
//!   headers, sections, names, bytes, symbol tables) can be walked
//!   without a panic or an out-of-bounds read.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG mutates
//! a hand-assembled valid ELF64 template and mixes in pure noise. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to
//! extend the loop to a wall-clock budget.

use tairix_binfmt::elf::ElfView;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte string fed to the decoder.
const MAX_NOISE: usize = 1024;

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A minimal valid ELF64: header, one program header, a `.shstrtab`, and
/// a three-entry section table (null, `.text`, `.shstrtab`).
fn valid_elf() -> Vec<u8> {
    const SHSTRTAB: &[u8] = b"\0.text\0.shstrtab\0";
    let phoff = 64u64;
    let text_off = phoff + 56;
    let text_len = 8u64;
    let shstrtab_off = text_off + text_len;
    let shoff = shstrtab_off + SHSTRTAB.len() as u64;

    let mut out = Vec::new();
    out.extend_from_slice(b"\x7fELF");
    out.extend_from_slice(&[2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    push_u16(&mut out, 2); // e_type
    push_u16(&mut out, 183); // e_machine: EM_AARCH64
    push_u32(&mut out, 1); // e_version
    push_u64(&mut out, 0x1_0000); // e_entry
    push_u64(&mut out, phoff);
    push_u64(&mut out, shoff);
    push_u32(&mut out, 0); // e_flags
    push_u16(&mut out, 64); // e_ehsize
    push_u16(&mut out, 56); // e_phentsize
    push_u16(&mut out, 1); // e_phnum
    push_u16(&mut out, 64); // e_shentsize
    push_u16(&mut out, 3); // e_shnum
    push_u16(&mut out, 2); // e_shstrndx

    // Program header: PT_LOAD, R+X.
    push_u32(&mut out, 1);
    push_u32(&mut out, 0b101);
    push_u64(&mut out, text_off);
    push_u64(&mut out, 0x1_0000);
    push_u64(&mut out, 0x1_0000);
    push_u64(&mut out, text_len);
    push_u64(&mut out, text_len);
    push_u64(&mut out, 0x1000);

    out.extend_from_slice(&[0xD5, 0x03, 0x20, 0x1F, 0xC0, 0x03, 0x5F, 0xD6]);
    out.extend_from_slice(SHSTRTAB);

    // Sections: null, .text (PROGBITS), .shstrtab (STRTAB).
    let sections: [(u32, u32, u64, u64); 3] = [
        (0, 0, 0, 0),
        (1, 1, text_off, text_len),
        (7, 3, shstrtab_off, SHSTRTAB.len() as u64),
    ];
    for (name, sh_type, offset, size) in sections {
        push_u32(&mut out, name);
        push_u32(&mut out, sh_type);
        push_u64(&mut out, 0); // flags
        push_u64(&mut out, 0); // addr
        push_u64(&mut out, offset);
        push_u64(&mut out, size);
        push_u32(&mut out, 0); // link
        push_u32(&mut out, 0); // info
        push_u64(&mut out, 1); // align
        push_u64(&mut out, 0); // entsize
    }
    out
}

/// Decode `bytes`; a success must be walkable without a panic.
fn exercise(bytes: &[u8]) {
    let Ok(view) = ElfView::parse(bytes) else {
        return;
    };
    for i in 0..view.header().phnum {
        let _ = view.program_header(i);
    }
    for i in 0..view.header().shnum {
        if let Ok(section) = view.section(i) {
            let _ = view.section_name(&section);
            let _ = view.section_bytes(&section);
        }
        if let Ok(table) = view.symbol_table(i) {
            for s in 0..table.len() {
                if let Ok(sym) = table.symbol(s) {
                    let _ = table.name(&sym);
                }
            }
        }
    }
}

#[test]
fn parse_never_panics_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "parse_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let template = valid_elf();

    let mut iteration: u64 = 0;
    loop {
        // 1. The valid template with a handful of bytes flipped.
        let mut mutated = template.clone();
        for _ in 0..bounded(next(), 8) {
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise(&mutated);

        // 2. The same, truncated or extended at random.
        let cut = bounded(next(), mutated.len());
        exercise(&mutated[..cut]);
        mutated.extend((0..bounded(next(), 64)).map(|_| low_byte(next() >> 23)));
        exercise(&mutated);

        // 3. Pure noise, optionally forced to open with the ELF magic.
        let mut noise: Vec<u8> = (0..bounded(next(), MAX_NOISE))
            .map(|_| low_byte(next() >> 29))
            .collect();
        if noise.len() >= 6 && next() & 1 == 0 {
            noise[..6].copy_from_slice(b"\x7fELF\x02\x01");
        }
        exercise(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
