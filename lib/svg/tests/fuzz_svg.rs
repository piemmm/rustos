//! Deterministic fuzz harness for the SVG decoder
//! (the desktop's untrusted image-decoding parser).
//!
//! [`tairix_svg::decode`] parses on-disk `/System/Graphics` assets that, on a
//! real system, may have been written or corrupted by anything. Per that
//! decode path is driven by a fuzz harness whose single invariant is:
//!
//! * `decode` never panics for any input — it returns `Ok` for a document in
//!   the supported subset and `Err` (fail closed) for everything else.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded
//! LCG draws pseudo-random byte strings, mutates real SVG templates, and
//! assembles structured-but-hostile documents. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz --soak` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use tairix_svg::decode;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_NOISE: usize = 4096;

/// Real templates the harness mutates: each exercises a different decode path.
const TEMPLATES: &[&[u8]] = &[
    br##"<svg viewBox="0 0 24 24"><polygon points="2,2 22,2 12,22" fill="#ff8800"/></svg>"##,
    br##"<svg viewBox="0 0 16 16"><path d="M2 2 h10 v10 h-10 Z" fill="#0a0"/></svg>"##,
    br##"<svg viewBox="0 0 20 20"><rect x="3" y="4" width="10" height="6" fill="#112233"/></svg>"##,
    br##"<svg width="32px" height="32px" data-hotspot-x="1" data-hotspot-y="2"><polygon points="0,0 32,0 32,32" fill="#fff" fill-opacity="0.5"/></svg>"##,
    br#"<?xml version="1.0"?><!-- c --><svg viewBox="0 0 8 8"><polygon points="0,0 8,0 8,8" fill="none"/></svg>"#,
    br##"<svg viewBox="0 0 24 24"><path d="M2 12 C2 2 22 2 22 12 S12 26 2 12 Z" fill="#345"/></svg>"##,
    br#"<svg viewBox="0 0 24 24"><path d="M4 12 A8 8 0 1 1 20 12 a4 4 0 1 0 -8 0 Q12 4 20 4 T4 12 z"/></svg>"#,
    br#"<svg viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="20" rx="5" ry="3"
            fill="none" stroke="hsl(210 50% 40%)" stroke-width="2"
            stroke-linejoin="round" stroke-linecap="square"
            stroke-dasharray="4 2 1" stroke-dashoffset="-3"/></svg>"#,
    br#"<svg viewBox="0 0 24 24"><g transform="translate(2 3) rotate(30 12 12) scale(1.5)"
            style="fill:rgb(10% 20% 30% / 0.5);stroke:currentColor" color="rebeccapurple">
            <circle cx="8" cy="8" r="5"/><ellipse cx="4" cy="4" rx="3" ry="auto"/>
            <line x1="0" y1="0" x2="9" y2="9"/></g></svg>"#,
    br##"<svg viewBox="0 0 24 24"><defs>
            <linearGradient id="a" x1="0" y1="0" x2="1" y2="1" spreadMethod="reflect">
              <stop offset="0" stop-color="#f00"/><stop offset="60%" stop-color="#0f0" stop-opacity="0.25"/>
            </linearGradient>
            <radialGradient id="b" href="#a" cx="0.3" cy="0.7" r="0.6" fx="0.1" fy="0.9"
              gradientUnits="objectBoundingBox" gradientTransform="skewX(20)"/>
            <symbol id="s"><rect width="4" height="4" fill="url(#b) #123"/></symbol>
          </defs>
          <use href="#s" x="3" y="4"/><use xlink:href="#s"/>
          <rect width="24" height="24" fill="url(#a)" opacity="0.75"/></svg>"##,
    br#"<svg viewBox="0 0 40 10" preserveAspectRatio="xMinYMax slice">
            <switch><g requiredExtensions="urn:x"><rect width="9" height="9"/></g>
            <svg x="1" y="1" width="8" height="8" viewBox="0 0 4 4"><rect width="4" height="4"/></svg>
            </switch></svg>"#,
];

/// Path-data command letters, so a generated `d` reaches every arm of the
/// grammar rather than only the ones a byte flip happens to spell.
const COMMANDS: &[u8] = b"MmLlHhVvCcSsQqTtAaZz";

/// Number spellings a real document uses, including the awkward ones the
/// separator rules turn on.
const NUMBERS: &[&str] = &[
    "0", "1", "-1", ".5", "-.5", "7.", "1e2", "-3E-2", "1.5", "1000000", "-0", "0.0001",
];

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Decode arbitrary bytes: must never panic, and anything it accepts must be
/// artwork a consumer can draw — no layer without contours, and no more
/// vertices than the decoder's own bound admits.
fn decode_never_panics(bytes: &[u8]) {
    if let Ok(image) = decode(bytes) {
        let mut vertices = 0usize;
        for layer in image.layers() {
            assert!(!layer.contours.is_empty(), "a layer with nothing to fill");
            vertices += layer.contours.iter().map(Vec::len).sum::<usize>();
        }
        assert!(vertices <= 65_536, "{vertices} vertices past the bound");
    }
}

/// One generated body inside a fixed, well-formed frame.
fn alloc_document(body: &str) -> String {
    format!(r#"<svg viewBox="0 0 24 24">{body}</svg>"#)
}

#[test]
fn decode_never_panics_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `tairix_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut state: u64 = tairix_fuzzseed::start(
        "decode_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A real template with a handful of bytes flipped at random.
        let template = TEMPLATES[bounded(next(), TEMPLATES.len() - 1)];
        let mut mutated = template.to_vec();
        let flips = bounded(next(), 8);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        decode_never_panics(&mutated);

        // 2. A structured-but-hostile document: a valid frame with a random
        //    blob spliced into the middle, exercising the element scanner.
        let blob_len = bounded(next(), 64);
        let blob: Vec<u8> = (0..blob_len).map(|_| low_byte(next() >> 23)).collect();
        let mut spliced = Vec::new();
        spliced.extend_from_slice(br#"<svg viewBox="0 0 16 16">"#);
        spliced.extend_from_slice(&blob);
        spliced.extend_from_slice(br#"<polygon points="0,0 16,0 16,16"/></svg>"#);
        decode_never_panics(&spliced);

        // 3. A generated path: the grammar's own alphabet, so the parser's
        //    curve, arc, and reflection arms are reached deliberately rather
        //    than by a lucky byte flip.
        let mut data = String::from("M0 0");
        let steps = bounded(next(), 12);
        for _ in 0..steps {
            let command = COMMANDS[bounded(next(), COMMANDS.len() - 1)];
            data.push(char::from(command));
            let arity = bounded(next(), 7);
            for _ in 0..arity {
                data.push_str(NUMBERS[bounded(next(), NUMBERS.len() - 1)]);
                match bounded(next(), 3) {
                    0 => data.push(' '),
                    1 => data.push(','),
                    _ => {}
                }
            }
        }
        let generated = alloc_document(&format!(
            r##"<path d="{data}" fill="#345" stroke="#987" stroke-width="0.4"/>"##
        ));
        decode_never_panics(generated.as_bytes());

        // 4. Pure noise straight into the decoder.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        decode_never_panics(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
