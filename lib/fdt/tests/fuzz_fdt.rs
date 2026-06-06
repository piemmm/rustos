//! Deterministic fuzz harness for the `lib/fdt` device-tree reader
//! (`AGENTS.md` §19.5 / §19.6 — a parser of boot-supplied, untrusted input).
//!
//! A flattened device tree is the hardware description firmware or a
//! bootloader hands the kernel ([`rustos_fdt::Fdt::new`]); the aarch64 and
//! riscv64 ports build their §18.2 platform discovery on it. Those bytes are
//! outside RustOS's trust boundary: a malformed header, a structure-block
//! offset that escapes the blob, an unterminated node name, or a property
//! length that runs past the value must all be **rejected**, never trusted
//! (`AGENTS.md` §5.4 — fail closed). Per §19.6 ("every parser of untrusted
//! input ... has a fuzz target") that decode path is driven here against
//! arbitrary device trees, with a single invariant:
//!
//! * feeding any byte stream to [`rustos_fdt::Fdt::new`] and draining every
//!   public reader ([`rustos_fdt::Fdt::first_memory_region`],
//!   `timebase_frequency`, `each_cpu`, `property`, `property_u64`, the node
//!   and property iterators) never panics and never reads out of bounds — the
//!   reader either returns a well-formed view or an [`rustos_fdt::FdtError`]
//!   (fail closed, `AGENTS.md` §2.9). The run aborting *is* the failure.
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a fixed-seed
//! LCG draws pseudo-random byte strings, flips bytes inside real device trees
//! built by the shared `fixture` builder (`AGENTS.md` §2.2 — one DTB builder,
//! not a second one rolled here), and splices a valid 40-byte header onto a
//! hostile structure block. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use rustos_fdt::fixture::{arm_with_cpus, virt_like, DtbBuilder};
use rustos_fdt::Fdt;

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string fed straight to the reader.
const MAX_NOISE: usize = 4096;

/// Node paths probed via [`Fdt::property`] / [`Fdt::property_u64`]; a mix of
/// present and absent paths exercises both the hit and miss walks.
const PROBE_PATHS: &[(&[&[u8]], &[u8])] = &[
    (&[b"cpus"], b"timebase-frequency"),
    (&[b"cpus"], b"#address-cells"),
    (&[b"memory@80000000"], b"device_type"),
    (&[b"memory@80000000"], b"reg"),
    (&[], b"#address-cells"),
    (&[b"absent"], b"missing"),
];

/// Build the corpus of real, well-formed device trees the harness mutates.
/// They all come from the shared `fixture` builder so this harness adds no
/// second DTB layout (`AGENTS.md` §2.2).
fn templates() -> Vec<Vec<u8>> {
    vec![
        virt_like(0x8000_0000, 0x4000_0000, 10_000_000),
        virt_like(0, 0, 0),
        arm_with_cpus(
            0x4000_0000,
            0x8000_0000,
            &[(0x0, Some(1024)), (0x1, None), (0x100, Some(512))],
        ),
        arm_with_cpus(0x8000_0000, 0x1000_0000, &[]),
        {
            // A deeply nested tree with assorted property shapes, to drive the
            // node/property iterators and the cell decoders.
            let mut b = DtbBuilder::new();
            b.begin_node("");
            b.prop_u32("#address-cells", 2);
            b.prop_u32("#size-cells", 2);
            b.begin_node("soc");
            b.prop_str("compatible", "simple-bus");
            b.begin_node("virtio_mmio@a000000");
            b.prop_str("compatible", "virtio,mmio");
            b.prop("reg", &0xa00_0000u64.to_be_bytes());
            b.end_node();
            b.end_node();
            b.end_node();
            b.build()
        },
    ]
}

/// Deadline for the current run, or `None` for the fixed smoke sweep.
fn budget() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// Initial PRNG seed for this harness. `cargo xtask fuzz` exports
/// `RUSTOS_FUZZ_SEED` so each soak run explores fresh inputs (`AGENTS.md`
/// §19.6 / §2.1); a plain `cargo test` leaves it unset and replays the fixed
/// `salt` for a reproducible smoke sweep. `salt` distinguishes independent
/// PRNG streams within one harness.
fn seed(salt: u64) -> u64 {
    match std::env::var("RUSTOS_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(env) => env ^ salt,
        None => salt,
    }
}

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Parse `bytes` and drain every public reader: must never panic, whatever the
/// blob, and any structural defect must surface as a returned `FdtError` or a
/// `None`/empty iterator rather than an out-of-bounds read.
fn exercise_never_panics(bytes: &[u8]) {
    let Ok(fdt) = Fdt::new(bytes) else {
        return;
    };

    let _ = fdt.first_memory_region();
    let _ = fdt.timebase_frequency();

    // `each_cpu` returns `Err` on a malformed tree; either way it must not
    // panic. Accumulate to keep the closure side-effecting.
    let mut cpu_acc = 0u64;
    let _ = fdt.each_cpu(|mpidr, capacity| {
        cpu_acc = cpu_acc
            .wrapping_add(mpidr)
            .wrapping_add(capacity.unwrap_or(0));
    });
    let _ = cpu_acc;

    for (path, name) in PROBE_PATHS {
        let _ = fdt.property(path, name);
        let _ = fdt.property_u64(path, name);
    }

    // Walk the whole tree, touching every node and property accessor, so a
    // corrupted token, name, or property length is forced through the
    // iterators' bounds checks. A malformed token surfaces as an `Err` item
    // (fail closed); past it the walk simply stops.
    for node in fdt.nodes() {
        let Ok(node) = node else {
            break;
        };
        let _ = node.name();
        let _ = node.depth();
        let _ = node.is_compatible("virtio,mmio");
        for prop in node.properties() {
            let Ok(prop) = prop else {
                break;
            };
            let _ = prop.name();
            let value = prop.value();
            for off in [0usize, 1, value.len(), value.len().saturating_sub(1)] {
                let _ = prop.read_be_u32(off);
                let _ = prop.read_be_u64(off);
            }
            let mut strings = 0u64;
            for s in prop.iter_strings() {
                strings = strings.wrapping_add(u64::try_from(s.len()).unwrap_or(0));
            }
            let _ = strings;
        }
    }
}

#[test]
fn parsing_any_device_tree_never_panics() {
    let deadline = budget();
    let corpus = templates();

    // The LCG seed comes from `seed()`: fixed under a plain `cargo test`
    // (reproducible), fresh per soak run under `cargo xtask fuzz`.
    let mut state: u64 = seed(0x5DEE_CE66_D9B4_1AB5);
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A real device tree with a handful of bytes flipped at random,
        //    hammering the header offsets, token stream, and string block.
        let template = &corpus[bounded(next(), corpus.len() - 1)];
        let mut mutated = template.clone();
        let flips = bounded(next(), 12);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise_never_panics(&mutated);

        // 2. A truncation of a real tree: a header that promises more blob
        //    than is present, driving the bounds checks in `Fdt::new` and the
        //    iterators.
        let keep = bounded(next(), template.len());
        exercise_never_panics(&template[..keep]);

        // 3. A structured-but-hostile blob: a valid 40-byte FDT magic header
        //    over a random structure/strings region, so the reader accepts the
        //    header and then meets an adversarial token stream.
        let blob_len = bounded(next(), 256);
        let mut spliced = template[..40.min(template.len())].to_vec();
        for _ in 0..blob_len {
            spliced.push(low_byte(next() >> 23));
        }
        exercise_never_panics(&spliced);

        // 4. Pure noise straight into the reader.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        exercise_never_panics(&noise);

        iteration += 1;
        if !within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
