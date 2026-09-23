//! Deterministic fuzz harness for the `lib/binfmt` wasm module-structure
//! view (a decoder of untrusted executable-file bytes).
//!
//! [`tairix_binfmt::wasm::WasmView::parse`] decodes any file a viewer is
//! pointed at. The harness invariants:
//!
//! * parsing any byte string never panics — it returns a view or a typed
//!   error (fail closed);
//! * a successful parse yields a directory whose every payload, count,
//!   and function-body walk terminates without a panic (a hostile LEB128
//!   length cannot cause an overrun or an unbounded loop).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates
//! a hand-assembled valid module and mixes in pure noise. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to
//! extend the loop to a wall-clock budget.

use tairix_binfmt::wasm::WasmView;
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte string fed to the decoder.
const MAX_NOISE: usize = 1024;

/// A valid module: custom + type + function + two-body code section.
fn valid_module() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\0asm");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&[0, 5, 4, b'n', b'a', b'm', b'e']);
    out.extend_from_slice(&[1, 4, 0x01, 0x60, 0x00, 0x00]);
    out.extend_from_slice(&[3, 3, 0x02, 0x00, 0x00]);
    out.extend_from_slice(&[10, 10, 0x02, 3, 0x00, 0x01, 0x0B, 4, 0x00, 0x01, 0x01, 0x0B]);
    out
}

/// Decode `bytes`; a success must be walkable without a panic.
fn exercise(bytes: &[u8]) {
    let Ok(view) = WasmView::parse(bytes) else {
        return;
    };
    for entry in view.sections() {
        let _ = view.section_bytes(entry);
        let _ = view.entry_count(entry.id);
    }
    if let Ok(Some(bodies)) = view.code_bodies() {
        for body in bodies {
            let _ = body;
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

    let template = valid_module();

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

        // 3. Pure noise, optionally forced to open with the wasm header,
        //    exercising the directory walk and LEB reader on raw noise.
        let mut noise = vec![0u8; rng.at_most(MAX_NOISE)];
        rng.fill(&mut noise);
        if noise.len() >= 8 && rng.next_u64() & 1 == 0 {
            noise[..4].copy_from_slice(b"\0asm");
            noise[4..8].copy_from_slice(&1u32.to_le_bytes());
        }
        exercise(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
