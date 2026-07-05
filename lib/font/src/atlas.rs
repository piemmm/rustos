// GENERATED FILE — DO NOT EDIT.
//
// Emitted by `cargo xtask font-atlas --write` from the committed face
// `lib/font/assets/Inconsolata-Regular.ttf` (SIL OFL 1.1; see
// `lib/font/assets/OFL.txt`). `cargo xtask font-atlas` (run by `ci`)
// fails closed if this file drifts from a fresh generation
// (AGENTS.md §2.2: generated views are never hand-maintained).

//! The generated Inconsolata glyph atlas: fixed-cell 4-bit coverage
//! bitmaps for every codepoint the face maps, plus the codepoint →
//! cell-index range table. Pure data; the lookup and blitting live in
//! the hand-written modules of this crate.

/// Glyph cell width in pixels (the face's uniform half-em advance).
pub const CELL_WIDTH: u32 = 12;

/// Glyph cell height in pixels (ascent rows plus descent rows).
pub const CELL_HEIGHT: u32 = 26;

/// Baseline row: pixel rows above the baseline within a cell.
pub const BASELINE: u32 = 21;

/// Packed bytes per glyph cell (two 4-bit pixels per byte, rows padded
/// to whole bytes).
pub const BYTES_PER_CELL: usize = 156;

/// Cell index of the U+FFFD replacement character: the fallback for a
/// codepoint the face does not map.
pub const FALLBACK_INDEX: u32 = 881;

/// Total glyph cells in [`COVERAGE`].
pub const CELL_COUNT: u32 = 882;

/// The sorted, non-overlapping codepoint runs the atlas covers:
/// `(first, len, base)` maps codepoints `first..first + len` to the
/// consecutive cells starting at index `base`.
pub const RANGES: &[(u32, u32, u32)] = &[
    (0x0000, 1, 0),
    (0x000D, 1, 1),
    (0x0020, 95, 2),
    (0x00A0, 146, 97),
    (0x0134, 21, 243),
    (0x014A, 53, 264),
    (0x018F, 1, 317),
    (0x0192, 1, 318),
    (0x0198, 1, 319),
    (0x01A0, 2, 320),
    (0x01AF, 2, 322),
    (0x01B8, 2, 324),
    (0x01C7, 3, 326),
    (0x01E6, 2, 329),
    (0x01EA, 2, 331),
    (0x01FA, 34, 333),
    (0x022A, 4, 367),
    (0x0230, 4, 371),
    (0x0237, 1, 375),
    (0x024D, 1, 376),
    (0x0259, 1, 377),
    (0x027B, 1, 378),
    (0x0298, 1, 379),
    (0x029A, 1, 380),
    (0x02B9, 4, 381),
    (0x02BE, 2, 385),
    (0x02C6, 7, 387),
    (0x02D8, 6, 394),
    (0x0300, 5, 400),
    (0x0306, 7, 405),
    (0x030F, 1, 412),
    (0x0311, 2, 413),
    (0x031B, 1, 415),
    (0x0323, 2, 416),
    (0x0326, 3, 418),
    (0x032E, 1, 421),
    (0x0331, 1, 422),
    (0x0335, 2, 423),
    (0x0375, 1, 425),
    (0x1E08, 2, 426),
    (0x1E0C, 4, 428),
    (0x1E14, 4, 432),
    (0x1E1C, 2, 436),
    (0x1E20, 2, 438),
    (0x1E24, 2, 440),
    (0x1E2A, 2, 442),
    (0x1E2E, 2, 444),
    (0x1E36, 2, 446),
    (0x1E3A, 2, 448),
    (0x1E42, 8, 450),
    (0x1E4C, 8, 458),
    (0x1E5A, 2, 466),
    (0x1E5E, 12, 468),
    (0x1E6C, 4, 480),
    (0x1E78, 4, 484),
    (0x1E80, 6, 488),
    (0x1E8E, 2, 494),
    (0x1E92, 2, 496),
    (0x1E97, 1, 498),
    (0x1E9E, 1, 499),
    (0x1EA0, 90, 500),
    (0x2007, 5, 590),
    (0x2010, 1, 595),
    (0x2012, 4, 596),
    (0x2018, 3, 600),
    (0x201C, 3, 603),
    (0x2020, 3, 606),
    (0x2026, 1, 609),
    (0x2030, 1, 610),
    (0x2032, 2, 611),
    (0x2039, 2, 613),
    (0x2044, 1, 615),
    (0x2070, 1, 616),
    (0x2074, 6, 617),
    (0x207B, 1, 623),
    (0x207F, 11, 624),
    (0x20A1, 1, 635),
    (0x20A3, 2, 636),
    (0x20A6, 2, 638),
    (0x20A9, 1, 640),
    (0x20AB, 3, 641),
    (0x20B1, 2, 644),
    (0x20B5, 1, 646),
    (0x20B9, 2, 647),
    (0x20BC, 2, 649),
    (0x2113, 1, 651),
    (0x2116, 1, 652),
    (0x2122, 1, 653),
    (0x2124, 1, 654),
    (0x2126, 1, 655),
    (0x212E, 1, 656),
    (0x2190, 10, 657),
    (0x21E6, 5, 667),
    (0x2202, 1, 672),
    (0x2205, 2, 673),
    (0x2208, 1, 675),
    (0x220F, 1, 676),
    (0x2211, 2, 677),
    (0x2215, 1, 679),
    (0x2217, 1, 680),
    (0x2219, 2, 681),
    (0x221E, 1, 683),
    (0x222B, 1, 684),
    (0x2248, 1, 685),
    (0x2260, 1, 686),
    (0x2264, 2, 687),
    (0x2295, 1, 689),
    (0x2302, 1, 690),
    (0x2318, 1, 691),
    (0x2325, 3, 692),
    (0x232B, 1, 695),
    (0x238B, 1, 696),
    (0x23CE, 1, 697),
    (0x2423, 1, 698),
    (0x2500, 160, 699),
    (0x25C6, 2, 859),
    (0x25CA, 2, 861),
    (0x25CF, 1, 863),
    (0x2639, 3, 864),
    (0x2660, 1, 867),
    (0x2663, 1, 868),
    (0x2665, 2, 869),
    (0x2713, 3, 871),
    (0x2717, 2, 874),
    (0x2B05, 3, 876),
    (0x2B95, 1, 879),
    (0x2E12, 1, 880),
    (0xFFFD, 1, 881),
];

/// The packed coverage payload: [`CELL_COUNT`] cells of
/// [`BYTES_PER_CELL`] bytes each, in range order. Within a cell: row
/// major, two 4-bit pixels per byte, left pixel in the high nibble,
/// `0` transparent through `15` fully covered.
pub static COVERAGE: &[u8] = include_bytes!("atlas_coverage.bin");
