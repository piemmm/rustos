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

use core::fmt::Write as _;

use tairix_svg::{decode, SvgError, Viewport};

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
            <rect systemLanguage="zz" width="2" height="2"/>
            <svg x="1" y="1" width="8" height="8" viewBox="0 0 4 4"><rect width="4" height="4"/></svg>
            </switch></svg>"#,
    br##"<svg viewBox="0 0 24 24">
            <style>/* c */ rect, .k > g .j { fill: #123; stroke-width: 2 !important }
              #d { clip-rule: evenodd } * { paint-order: stroke }
              @media print { rect { fill: #fff } }</style>
            <clipPath id="c" clipPathUnits="objectBoundingBox"><circle cx=".5" cy=".5" r=".4"/></clipPath>
            <clipPath id="d" clip-path="url(#c)"><rect width="9" height="9"/><use href="#p"/></clipPath>
            <path id="p" d="M0 0 h8 v8 z"/>
            <g class="k"><g><rect class="j" width="20" height="20" clip-path="url(#d)"/></g></g>
          </svg>"##,
    br##"<svg viewBox="0 0 24 24">
            <pattern id="p" width="0.25" height="0.25" patternTransform="rotate(20) scale(1.5)">
              <circle cx="2" cy="2" r="1.5" fill="#c33"/></pattern>
            <pattern id="q" href="#p" patternUnits="userSpaceOnUse" width="6" height="6"
              viewBox="0 0 4 4" preserveAspectRatio="xMidYMid meet" overflow="visible"/>
            <rect width="24" height="24" fill="url(#q) #048" fill-opacity="0.6"/>
            <circle cx="12" cy="12" r="8" fill="url(#p)" stroke="#000" stroke-width="2"
              opacity="0.5"/></svg>"##,
    br##"<svg viewBox="0 0 24 24">
            <pattern id="s" width="6" height="6" patternUnits="userSpaceOnUse"
              overflow="visible">
              <circle cx="3" cy="3" r="5" fill="#2a6" fill-opacity="0.5"/>
              <rect x="-2" y="4" width="10" height="3" fill="#d51"/></pattern>
            <pattern id="t" href="#s" width="5" height="5" patternUnits="userSpaceOnUse"
              overflow="visible"/>
            <rect width="24" height="12" fill="url(#s)" fill-opacity="0.4"/>
            <circle cx="12" cy="18" r="6" fill="url(#t)"/></svg>"##,
    br##"<svg viewBox="0 0 16 16">
            <pattern id="a" width="4" height="4" patternUnits="userSpaceOnUse"
              patternContentUnits="objectBoundingBox">
              <rect width=".2" height=".2" fill="#0a0"/></pattern>
            <pattern id="b" width="8" height="8" patternUnits="userSpaceOnUse">
              <rect width="8" height="8" fill="url(#a)" clip-path="url(#c)"/>
              <path d="M0 0 L8 8" stroke="#a00" stroke-width="1"/></pattern>
            <clipPath id="c"><circle cx="4" cy="4" r="3"/></clipPath>
            <rect width="16" height="16" fill="url(#b)"/></svg>"##,
    br##"<svg viewBox="0 0 24 24">
            <marker id="a" markerWidth="4" markerHeight="4" refX="2" refY="2"
              orient="auto" viewBox="0 0 8 8" preserveAspectRatio="xMidYMid meet">
              <path d="M0 0 L8 4 L0 8 Z" fill="#c33"/></marker>
            <marker id="b" markerWidth="2" markerHeight="2" orient="auto-start-reverse"
              overflow="visible" markerUnits="userSpaceOnUse">
              <circle cx="1" cy="1" r="3" fill="#39c" fill-opacity="0.5"/></marker>
            <marker id="c" markerWidth="3" markerHeight="3" orient="45grad"/>
            <path d="M2 12 C2 2 22 2 22 12 S12 26 2 12 Z" fill="none" stroke="#345"
              stroke-width="2" marker-start="url(#b)" marker-mid="url(#a)"
              marker-end="url(#b)" paint-order="markers stroke fill"/>
            <polyline points="1,1 6,2 11,1 16,4" fill="none" stroke-width="0.5"
              style="marker:url(#a)" opacity="0.6"/>
            <polygon points="18,18 22,18 22,22" marker-mid="url(#c)"
              marker-end="url(#nothing)" stroke-width="3"/>
            <line x1="1" y1="20" x2="6" y2="20" marker-start="url(#a)"
              markerUnits="strokeWidth"/></svg>"##,
    br##"<svg viewBox="0 0 24 24">
            <mask id="m" maskContentUnits="objectBoundingBox" style="mask-type:alpha">
              <rect width=".5" height="1" fill="#fff"/></mask>
            <mask id="n" maskUnits="userSpaceOnUse" x="1" y="1" width="8" height="8" mask="url(#m)">
              <circle cx="12" cy="12" r="9" fill="#888"/></mask>
            <symbol id="s" viewBox="0 0 4 4" overflow="visible"><rect width="6" height="6"/></symbol>
            <g opacity="0.4" mask="url(#n)"><use href="#s" x="2" y="2" width="12" height="9"/>
              <rect width="9" height="9" fill="#357" stroke="#753" stroke-width="1" opacity=".5"/></g>
          </svg>"##,
];

/// Path-data command letters, so a generated `d` reaches every arm of the
/// grammar rather than only the ones a byte flip happens to spell.
const COMMANDS: &[u8] = b"MmLlHhVvCcSsQqTtAaZz";

/// Number spellings a real document uses, including the awkward ones the
/// separator rules turn on.
const NUMBERS: &[&str] = &[
    "0", "1", "-1", ".5", "-.5", "7.", "1e2", "-3E-2", "1.5", "1000000", "-0", "0.0001",
];

/// Selector spellings a generated sheet draws from, inside the subset and
/// outside it.
const SELECTORS: &[&str] = &[
    "*",
    "rect",
    ".j",
    "#r",
    "rect.j",
    ".k rect",
    ".k > .j",
    "g+rect",
    "rect:hover",
    "rect[x]",
    ".k > .j rect",
    "",
    ">rect",
];

/// Property names a generated sheet draws from: some the cascade reads, some
/// it ignores.
const PROPERTIES: &[&str] = &[
    "fill",
    "fill-opacity",
    "opacity",
    "stroke-width",
    "clip-path",
    "mask",
    "paint-order",
    "clip-rule",
    "overflow",
    "marker",
    "marker-mid",
    "font-family",
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

/// The pixel side the accepted artwork is rendered at.
///
/// Small on purpose: what is being checked is that the renderer accepts what
/// the decoder produced, which the geometry decides and the size does not.
const RENDER_SIDE: u32 = 16;

/// Decode arbitrary bytes: must never panic, and anything it accepts must be
/// artwork a consumer can draw — no layer without contours, no more vertices
/// or layers than the decoder's own bounds admit, and a tree the one renderer
/// will actually draw rather than turn away.
fn decode_never_panics(bytes: &[u8]) {
    let square = decode(bytes, Viewport::Square);
    let natural = decode(bytes, Viewport::Natural);
    // A viewport chooses the shape a drawing is fitted to, never whether
    // the document is well formed. The one thing it may legitimately change
    // is how much geometry the fit produces, and so whether that geometry
    // fits within the bound.
    assert!(
        square.is_ok() == natural.is_ok()
            || square == Err(SvgError::TooComplex)
            || natural == Err(SvgError::TooComplex),
        "the viewports disagreed other than about complexity"
    );
    for image in [square, natural].into_iter().flatten() {
        // Whatever the decoder accepts, the one renderer draws: a tree nested
        // past the renderer's bound, or a pattern whose tile cannot be
        // realised, would decode and then refuse to appear. Asserted first,
        // because it is also what bounds the walk below.
        let mut surface =
            tairix_raster::Surface::new(RENDER_SIDE, RENDER_SIDE).expect("a small surface");
        assert!(
            surface.draw_artwork(image.nodes(), image.design()),
            "the renderer refused artwork the decoder accepted"
        );
        let mut tally = Tally::default();
        tally.walk(image.nodes());
        assert!(
            tally.vertices <= 65_536,
            "{} vertices past the bound",
            tally.vertices
        );
        assert!(
            tally.layers <= 1024,
            "{} layers past the bound",
            tally.layers
        );
    }
}

/// What a decoded document charged against the decoder's budgets.
#[derive(Default)]
struct Tally {
    layers: usize,
    vertices: usize,
}

impl Tally {
    /// Count every filled layer, *including* the ones inside a pattern's own
    /// tile.
    ///
    /// The shared walk deliberately leaves a tile out, because a tile is a
    /// drawing in its own space rather than this one — but the decoder
    /// charges its geometry against the same budget, so the invariant under
    /// test has to see it.
    fn walk(&mut self, nodes: &[tairix_raster::Node]) {
        for node in nodes {
            match node {
                tairix_raster::Node::Fill(layer) => {
                    assert!(!layer.contours.is_empty(), "a layer with nothing to fill");
                    self.layers += 1;
                    self.vertices += layer.contours.iter().map(Vec::len).sum::<usize>();
                    if let tairix_raster::Paint::Pattern(pattern) = &layer.paint {
                        self.walk(&pattern.content);
                    }
                }
                tairix_raster::Node::Group(group) => {
                    self.walk(&group.children);
                    if let Some(mask) = &group.mask {
                        self.walk(&mask.content);
                    }
                }
            }
        }
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
        let orient = ["auto", "auto-start-reverse", "30", "-1.5rad", "2turn"][bounded(next(), 4)];
        let generated = alloc_document(&format!(
            r##"<marker id="k" markerWidth="2" markerHeight="2" refX="1" orient="{orient}"
                  overflow="visible"><rect width="3" height="3" fill="#0a0"/></marker>
                <path d="{data}" fill="#345" stroke="#987" stroke-width="0.4"
                  marker-start="url(#k)" marker-mid="url(#k)" marker-end="url(#k)"/>"##
        ));
        decode_never_panics(generated.as_bytes());

        // 4. A generated stylesheet: the selector and declaration grammar,
        //    so the cascade's own parser is reached deliberately.
        let mut sheet = String::new();
        let rules = bounded(next(), 6);
        for _ in 0..rules {
            let selector = SELECTORS[bounded(next(), SELECTORS.len() - 1)];
            let property = PROPERTIES[bounded(next(), PROPERTIES.len() - 1)];
            let value = NUMBERS[bounded(next(), NUMBERS.len() - 1)];
            let _ = write!(sheet, "{selector}{{{property}:{value}");
            match bounded(next(), 3) {
                0 => sheet.push_str("!important}"),
                1 => sheet.push_str(";}"),
                _ => sheet.push('}'),
            }
        }
        let styled = alloc_document(&format!(
            r#"<style>{sheet}</style><g class="k"><rect id="r" class="j" width="9" height="9"/></g>"#
        ));
        decode_never_panics(styled.as_bytes());

        // 5. Pure noise straight into the decoder.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        decode_never_panics(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
