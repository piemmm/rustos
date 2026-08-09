//! `cargo xtask font-atlas` implementation.
//!
//! This command rasterises the **console atlas**: the whole repertoire of the
//! console font family (`lib/font/assets/mono/`, SIL OFL 1.1) into a
//! fixed-cell, 4-bit-coverage bitmap atlas, emitted as generated data
//! (`lib/font/src/atlas.rs` + `lib/font/src/atlas_coverage.bin`) that the
//! framebuffer boot console (`lib/fbcon`) and the `lib/font` geometry
//! constants compile in. With no arguments the command verifies the committed
//! atlas matches a fresh generation (the `ci` drift guard, exactly like
//! `c-header`); `--write` regenerates it.
//!
//! Every face the family's `FontFamily` manifest names is compiled in, in its
//! resolution order: the primary Latin/Greek/Cyrillic face and its CJK
//! (M PLUS 1 Code, `D2Coding`) and Hebrew (Noto Sans Hebrew) companions. The
//! text console runs in the kernel and has no way to ask a user-space service
//! for a glyph, so its repertoire is whatever is compiled in: a companion
//! left out is a script the console simply cannot draw, at a `man` page, a
//! login prompt or a panic alike. The `fontd` font service reads the same
//! faces from `/System/Fonts` at runtime for scalable, proportional text
//! through this same `lib/fontface` engine, so the two views share one source
//! and one rasteriser rather than each carrying its own.
//!
//! The face list itself is read from the family manifest through the shared
//! [`font_store`] reader the image builder plants the store with — this
//! command never names a face.
//!
//! The generator is deliberately first-party and deterministic: the shared
//! `lib/fontface` engine (a minimal TrueType reader
//! `head`/`maxp`/`cmap` format 4/`hhea`/`hmtx`/`loca`/`glyf` and a scanline
//! rasteriser — quadratic outlines flattened to segments, non-zero winding,
//! 4 sample rows per pixel row with exact horizontal span coverage) turns each
//! outline into coverage. That one engine also renders the font service's
//! glyphs, so the console atlas and live text share one rasteriser rather
//! than each carrying its own. Identical input bytes produce identical output
//! bytes on every host, so the drift guard is meaningful.
//!
//! Pixel geometry: the em square is `tairix_fontface::ATLAS_EM_PX` pixels
//! tall. Inconsolata is
//! strictly monospace: every spacing glyph advances by one uniform width the
//! generator reads from the face itself, and that advance defines the terminal
//! cell. Every bitmap slot is two cells wide so the compiled-in glyph bitmap
//! format is identical to the wide-glyph format the shared decode/blit path
//! (`glyph.rs`, `font.rs`, `lib/fbcon`) already handles — one glyph format for
//! both the compiled-in console glyphs and the wide glyphs the font service
//! serves, never a second; a primary Latin glyph leaves the continuation cell
//! transparent. Cell height and baseline derive from the primary face.
//! Zero-advance combining marks rasterise like
//! any other glyph — the face draws their outlines inside the advance-wide
//! cell (GPOS anchor repositioning is a shaping concern the cell grid
//! deliberately does not have), so each mark lands in its own cell: the same
//! deliberate one-scalar-per-cell model `lib/vt` / `lib/curses` document.

use std::fmt::Write as _;
use std::path::Path;

use tairix_fontface::{CellGeometry, FontError, FontFamily, ATLAS_EM_PX};

use super::{font_lineart, font_store};

/// Workspace-relative path of the generated Rust atlas view.
pub const DEFAULT_ATLAS_RS_PATH: &str = "lib/font/src/atlas.rs";
/// Workspace-relative path of the generated coverage payload.
pub const DEFAULT_ATLAS_BIN_PATH: &str = "lib/font/src/atlas_coverage.bin";

/// Maximum terminal cells occupied by one generated glyph bitmap.
const MAX_GLYPH_CELLS: u32 = 2;

/// Maximum literal run represented by one atlas compression token.
const MAX_LITERAL_LEN: usize = 128;

/// Minimum repeated byte sequence represented by a back-reference.
const MIN_MATCH_LEN: usize = 3;

/// Maximum repeated byte sequence represented by one back-reference.
const MAX_MATCH_LEN: usize = 34;

/// Maximum backwards distance represented by a back-reference.
const MAX_MATCH_DISTANCE: usize = 1024;

/// Render a shared-engine [`FontError`] as this command's `String` error.
fn engine_err(error: FontError) -> String {
    format!("font-atlas: {error}")
}

/// A contiguous run of mapped codepoints, indexing consecutive glyph cells.
struct AtlasRange {
    first: u32,
    len: u32,
    base: u32,
}

/// The fully built atlas, ready for emission.
struct Atlas {
    geometry: CellGeometry,
    ranges: Vec<AtlasRange>,
    /// One two-cell-capable bitmap of unpacked 4-bit coverage values per
    /// mapped codepoint, in codepoint order.
    cells: Vec<Vec<u8>>,
    /// Index of the U+FFFD replacement-character cell.
    fallback: u32,
}

/// Rasterise an ordered family of faces into an [`Atlas`].
///
/// For an overlapping codepoint, the earliest face supplies the outline.
fn build_atlas(faces: &[&[u8]]) -> Result<Atlas, String> {
    let family = FontFamily::parse(faces).map_err(engine_err)?;
    let primary = family.primary();
    let advance = primary.uniform_advance().map_err(engine_err)?;
    let geometry = CellGeometry::derive(primary, advance, ATLAS_EM_PX).map_err(engine_err)?;
    let glyph_width = geometry.width * MAX_GLYPH_CELLS;
    // The merged repertoire is walked in codepoint order, the earliest face
    // winning an overlap — exactly the cell order the emitted ranges require.
    // A zero-advance combining mark rasterises like any other glyph: the face
    // draws its outline inside the advance-wide cell, so each mark lands in its
    // own cell — the one-scalar-per-cell model the terminal stack documents.
    let merged = family.merged();
    let mut ranges: Vec<AtlasRange> = Vec::new();
    let mut cells = Vec::with_capacity(merged.len());
    let mut fallback = None;
    for (code, face_index, glyph) in merged {
        // Box Drawing and Block Elements are drawn to the pixel grid rather
        // than rasterised, because they have to tile.
        let cell = match font_lineart::coverage(code, geometry.width, geometry.height) {
            Some(synthesised) => widen(&synthesised, geometry.width, glyph_width),
            None => family
                .rasterise(
                    face_index,
                    glyph,
                    &geometry,
                    f64::from(ATLAS_EM_PX),
                    glyph_width,
                )
                .map_err(engine_err)?,
        };
        let index = u32::try_from(cells.len())
            .map_err(|_| "font-atlas: more mapped codepoints than fit a u32 index".to_owned())?;
        if code == u32::from(char::REPLACEMENT_CHARACTER) {
            fallback = Some(index);
        }
        match ranges.last_mut() {
            Some(range) if range.first + range.len == code => range.len += 1,
            _ => ranges.push(AtlasRange {
                first: code,
                len: 1,
                base: index,
            }),
        }
        cells.push(cell);
    }
    let fallback = fallback.ok_or_else(|| {
        "font-atlas: face does not map U+FFFD, so there is no fallback glyph".to_owned()
    })?;
    Ok(Atlas {
        geometry,
        ranges,
        cells,
        fallback,
    })
}

/// Place a one-cell bitmap in the two-cell-capable one the atlas stores,
/// leaving the second cell transparent.
fn widen(cell: &[u8], width: u32, glyph_width: u32) -> Vec<u8> {
    let mut widened = vec![0u8; (glyph_width as usize) * (cell.len() / width as usize)];
    for (row, source) in cell.chunks(width as usize).enumerate() {
        let start = row * glyph_width as usize;
        widened[start..start + source.len()].copy_from_slice(source);
    }
    widened
}

impl Atlas {
    /// Packed bytes per glyph bitmap: two 4-bit pixels per byte, rows padded
    /// to whole bytes.
    fn bytes_per_glyph(&self) -> usize {
        (self.geometry.width as usize * MAX_GLYPH_CELLS as usize).div_ceil(2)
            * self.geometry.height as usize
    }

    /// The coverage payload: a little-endian offset table followed by one
    /// independently compressed block per glyph. Independent blocks keep
    /// lookup bounded to one glyph and make a corrupt block unable to affect
    /// its neighbours.
    fn coverage_bytes(&self) -> Result<Vec<u8>, String> {
        let width = self.geometry.width as usize * MAX_GLYPH_CELLS as usize;
        let mut compressed = Vec::new();
        let mut offsets = Vec::with_capacity(self.cells.len() + 1);
        offsets.push(0u32);
        for cell in &self.cells {
            let mut packed = Vec::with_capacity(self.bytes_per_glyph());
            for row in cell.chunks(width) {
                for pair in row.chunks(2) {
                    let high = pair[0] << 4;
                    let low = pair.get(1).copied().unwrap_or(0);
                    packed.push(high | low);
                }
            }
            compress_glyph(&packed, &mut compressed)?;
            offsets
                .push(u32::try_from(compressed.len()).map_err(|_| {
                    "font-atlas: compressed payload exceeds u32 offsets".to_owned()
                })?);
        }
        let table_bytes = offsets.len() * size_of::<u32>();
        let mut bytes = Vec::with_capacity(table_bytes + compressed.len());
        for offset in offsets {
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        bytes.extend_from_slice(&compressed);
        Ok(bytes)
    }

    /// Render the generated Rust view (`lib/font/src/atlas.rs`).
    fn render_rust(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "// GENERATED FILE — DO NOT EDIT.\n\
             //\n\
             // Emitted by `cargo xtask font-atlas --write` from the committed\n\
             // console family `lib/font/assets/mono/` — every face its\n\
             // `FontFamily` manifest names (SIL OFL 1.1; see the `OFL-*.txt`\n\
             // files beside them).\n\
             // `cargo xtask font-atlas` (run by `ci`)\n\
             // fails closed if this file drifts from a fresh generation\n\
             // (AGENTS.md §2.2: generated views are never hand-maintained).\n\n",
        );
        out.push_str(
            "//! The generated **console atlas**: fixed-cell 4-bit coverage bitmaps\n\
             //! for the whole repertoire of the console font family — Latin, Greek,\n\
             //! Cyrillic, box drawing, arrows, punctuation, currency and U+FFFD from\n\
             //! the primary face, plus the CJK and Hebrew companions — with the\n\
             //! codepoint → glyph-index range table. The text console runs in the\n\
             //! kernel and cannot ask the font service for a glyph, so what is here\n\
             //! is what it can draw; a scalar outside this repertoire renders U+FFFD.\n\
             //! Pure data; lookup and blitting live in the hand-written modules of\n\
             //! this crate.\n\n",
        );
        let _ = writeln!(
            out,
            "/// Glyph cell width in pixels (the face's uniform advance, rounded to\n\
             /// whole pixels).\n\
             pub const CELL_WIDTH: u32 = {};\n",
            self.geometry.width
        );
        let _ = writeln!(
            out,
            "/// Maximum glyph bitmap width in pixels. Wide glyphs may cover both\n\
             /// terminal cells; narrow glyphs leave the second cell transparent.\n\
             pub const GLYPH_WIDTH: u32 = {};\n",
            self.geometry.width * MAX_GLYPH_CELLS
        );
        let _ = writeln!(
            out,
            "/// Glyph cell height in pixels (ascent rows plus descent rows).\n\
             pub const CELL_HEIGHT: u32 = {};\n",
            self.geometry.height
        );
        let _ = writeln!(
            out,
            "/// Baseline row: pixel rows above the baseline within a cell.\n\
             pub const BASELINE: u32 = {};\n",
            self.geometry.baseline
        );
        let _ = writeln!(
            out,
            "/// Packed bytes per glyph cell (two 4-bit pixels per byte, rows padded\n\
             /// to whole bytes).\n\
             pub const BYTES_PER_CELL: usize = {};\n",
            (self.geometry.width as usize).div_ceil(2) * self.geometry.height as usize
        );
        let _ = writeln!(
            out,
            "/// Packed bytes per two-cell-capable glyph bitmap.\n\
             pub const BYTES_PER_GLYPH: usize = {};\n",
            self.bytes_per_glyph()
        );
        let _ = writeln!(
            out,
            "/// Cell index of the U+FFFD replacement character: the fallback for a\n\
             /// codepoint the face does not map.\n\
             pub const FALLBACK_INDEX: u32 = {};\n",
            self.fallback
        );
        let _ = writeln!(
            out,
            "/// Total glyph bitmaps in [`COVERAGE`].\n\
             pub const CELL_COUNT: u32 = {};\n",
            self.cells.len()
        );
        out.push_str(
            "/// The sorted, non-overlapping codepoint runs the atlas covers:\n\
             /// `(first, len, base)` maps codepoints `first..first + len` to the\n\
             /// consecutive cells starting at index `base`.\n\
             pub const RANGES: &[(u32, u32, u32)] = &[\n",
        );
        for range in &self.ranges {
            let _ = writeln!(
                out,
                "    (0x{:04X}, {}, {}),",
                range.first, range.len, range.base
            );
        }
        out.push_str("];\n\n");
        out.push_str(
            "/// The coverage payload: a `(CELL_COUNT + 1)`-entry little-endian\n\
             /// `u32` offset table followed by one independently compressed block per\n\
             /// glyph. Decoded bitmaps are [`BYTES_PER_GLYPH`] bytes in range order:\n\
             /// row major, two 4-bit pixels per byte, left pixel in the high nibble,\n\
             /// `0` transparent through `15` fully covered.\n\
             pub static COVERAGE: &[u8] = include_bytes!(\"atlas_coverage.bin\");\n",
        );
        out
    }
}

/// Compress one packed glyph with a bounded LZ token stream.
///
/// A token below `0x80` is followed by `token + 1` literal bytes. A token at
/// or above `0x80` and its following byte encode a 3–34 byte back-reference:
/// the token's low two bits and the following byte hold `distance - 1`, while
/// bits 2–6 hold `length - 3`. Matches never cross glyph boundaries.
fn compress_glyph(glyph: &[u8], output: &mut Vec<u8>) -> Result<(), String> {
    let mut literal_start = 0usize;
    let mut cursor = 0usize;
    while cursor < glyph.len() {
        let (distance, length) = best_match(glyph, cursor);
        if length < MIN_MATCH_LEN {
            cursor += 1;
            if cursor - literal_start == MAX_LITERAL_LEN {
                emit_literals(&glyph[literal_start..cursor], output)?;
                literal_start = cursor;
            }
            continue;
        }
        emit_literals(&glyph[literal_start..cursor], output)?;
        let distance_minus_one = distance - 1;
        let encoded_length = u8::try_from(length - MIN_MATCH_LEN)
            .map_err(|_| "font-atlas: match length exceeds token field".to_owned())?;
        let distance_high = u8::try_from(distance_minus_one >> 8)
            .map_err(|_| "font-atlas: match distance exceeds token field".to_owned())?;
        let distance_low = u8::try_from(distance_minus_one & 0xFF)
            .map_err(|_| "font-atlas: match distance low byte exceeds token field".to_owned())?;
        let token = 0x80 | (encoded_length << 2) | distance_high;
        output.push(token);
        output.push(distance_low);
        cursor += length;
        literal_start = cursor;
    }
    emit_literals(&glyph[literal_start..], output)?;
    Ok(())
}

/// Find the longest encodable match ending before `cursor`.
fn best_match(glyph: &[u8], cursor: usize) -> (usize, usize) {
    if cursor + MIN_MATCH_LEN > glyph.len() {
        return (0, 0);
    }
    let first_candidate = cursor.saturating_sub(MAX_MATCH_DISTANCE);
    let mut best_distance = 0usize;
    let mut best_length = 0usize;
    for candidate in (first_candidate..cursor).rev() {
        if glyph[candidate] != glyph[cursor] {
            continue;
        }
        let max_length = MAX_MATCH_LEN.min(glyph.len() - cursor);
        let mut length = 1usize;
        while length < max_length && glyph[candidate + length] == glyph[cursor + length] {
            length += 1;
        }
        if length > best_length {
            best_distance = cursor - candidate;
            best_length = length;
            if length == max_length {
                break;
            }
        }
    }
    (best_distance, best_length)
}

/// Emit a non-empty literal slice in chunks accepted by the decoder.
fn emit_literals(mut literals: &[u8], output: &mut Vec<u8>) -> Result<(), String> {
    while !literals.is_empty() {
        let length = literals.len().min(MAX_LITERAL_LEN);
        output.push(
            u8::try_from(length - 1)
                .map_err(|_| "font-atlas: literal length exceeds token field".to_owned())?,
        );
        output.extend_from_slice(&literals[..length]);
        literals = &literals[length..];
    }
    Ok(())
}

/// Generate the console atlas from the committed console family, returning
/// the two artefacts as `(rust_view, coverage_payload)`.
///
/// Every face the family's manifest names is compiled in, in its order, so
/// the console draws every script the shipped family covers.
fn generate(workspace_root: &Path) -> Result<(String, Vec<u8>), String> {
    let family = font_store::read_family(workspace_root, font_store::CONSOLE_FAMILY)?;
    let atlas = build_atlas(&family.face_bytes())?;
    Ok((atlas.render_rust(), atlas.coverage_bytes()?))
}

/// Regenerate the committed atlas artefacts in place (`--write`).
pub fn write(workspace_root: &Path) -> Result<(), String> {
    let (rust_view, coverage) = generate(workspace_root)?;
    let rs_path = workspace_root.join(DEFAULT_ATLAS_RS_PATH);
    let bin_path = workspace_root.join(DEFAULT_ATLAS_BIN_PATH);
    std::fs::write(&rs_path, rust_view)
        .map_err(|e| format!("font-atlas: cannot write {}: {e}", rs_path.display()))?;
    std::fs::write(&bin_path, coverage)
        .map_err(|e| format!("font-atlas: cannot write {}: {e}", bin_path.display()))?;
    Ok(())
}

/// Verify the committed atlas artefacts match a fresh generation (the `ci`
/// drift guard). Fails closed with the regeneration command on any mismatch.
pub fn check_sync(workspace_root: &Path) -> Result<(), String> {
    let (rust_view, coverage) = generate(workspace_root)?;
    let rs_path = workspace_root.join(DEFAULT_ATLAS_RS_PATH);
    let bin_path = workspace_root.join(DEFAULT_ATLAS_BIN_PATH);
    let drifted = |path: &Path| {
        format!(
            "font-atlas: `{}` is out of sync with `{}`; \
             run `cargo xtask font-atlas --write` and commit the result.",
            path.display(),
            "the committed font sources",
        )
    };
    let committed_rs = std::fs::read(&rs_path).map_err(|_| drifted(&rs_path))?;
    if committed_rs != rust_view.as_bytes() {
        return Err(drifted(&rs_path));
    }
    let committed_bin = std::fs::read(&bin_path).map_err(|_| drifted(&bin_path))?;
    if committed_bin != coverage {
        return Err(drifted(&bin_path));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root")
    }

    fn committed_family() -> font_store::Family {
        font_store::read_family(&workspace_root(), font_store::CONSOLE_FAMILY)
            .expect("committed console family")
    }

    fn committed_atlas() -> Atlas {
        build_atlas(&committed_family().face_bytes()).expect("atlas builds")
    }

    /// The coverage cell for `code`, unpacked to one nibble value per pixel.
    fn cell_of(atlas: &Atlas, code: u32) -> &[u8] {
        let range = atlas
            .ranges
            .iter()
            .find(|r| (r.first..r.first + r.len).contains(&code))
            .expect("code is mapped");
        let index = (range.base + (code - range.first)) as usize;
        &atlas.cells[index]
    }

    fn payload_offset(payload: &[u8], index: usize) -> usize {
        let start = index * size_of::<u32>();
        let bytes: [u8; 4] = payload[start..start + 4].try_into().expect("offset entry");
        u32::from_le_bytes(bytes) as usize
    }

    fn decode_for_test(mut encoded: &[u8], decoded_len: usize) -> Vec<u8> {
        let mut decoded = Vec::with_capacity(decoded_len);
        while let Some((&token, rest)) = encoded.split_first() {
            encoded = rest;
            if token < 0x80 {
                let length = usize::from(token) + 1;
                decoded.extend_from_slice(&encoded[..length]);
                encoded = &encoded[length..];
                continue;
            }
            let (&distance_low, rest) = encoded.split_first().expect("match distance");
            encoded = rest;
            let length = usize::from((token >> 2) & 0x1F) + MIN_MATCH_LEN;
            let distance = ((usize::from(token & 0x03) << 8) | usize::from(distance_low)) + 1;
            for _ in 0..length {
                decoded.push(decoded[decoded.len() - distance]);
            }
        }
        assert_eq!(decoded.len(), decoded_len, "decoded glyph length");
        decoded
    }

    fn packed_cell(cell: &[u8], width: usize) -> Vec<u8> {
        let mut packed = Vec::with_capacity(cell.len().div_ceil(2));
        for row in cell.chunks(width) {
            for pair in row.chunks(2) {
                packed.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
            }
        }
        packed
    }

    #[test]
    fn geometry_matches_the_face_metrics() {
        let atlas = committed_atlas();
        // Inconsolata EX: 1024 upm, ascent 939, descent 198, advance 613. At a
        // 14 px em those land on the 8×16 cell PC text consoles have used
        // since VGA, which is what makes the console's grid `width / 8` ×
        // `height / 16`.
        assert_eq!(atlas.geometry.width, 8);
        assert_eq!(atlas.geometry.height, 16);
        assert_eq!(atlas.geometry.baseline, 13);
        assert_eq!(atlas.bytes_per_glyph(), 8 * 16);
    }

    #[test]
    fn compressed_payload_round_trips_without_size_regression() {
        // The payload is compiled into the kernel image on every target, so
        // it is bounded. The whole family at the 8×16 cell is 1.71 MB, against
        // 3.54 MB for the same faces at the old 15×28 cell; this holds that
        // with headroom and catches a cell-size or compression regression.
        const CONSOLE_PAYLOAD_CEILING: usize = 2_000_000;

        let atlas = committed_atlas();
        let payload = atlas.coverage_bytes().expect("coverage encoding succeeds");
        assert!(
            payload.len() <= CONSOLE_PAYLOAD_CEILING,
            "compressed payload grew to {} bytes",
            payload.len()
        );
        let table_len = (atlas.cells.len() + 1) * size_of::<u32>();
        let blocks = &payload[table_len..];
        let width = atlas.geometry.width as usize * MAX_GLYPH_CELLS as usize;
        for (index, cell) in atlas.cells.iter().enumerate() {
            let start = payload_offset(&payload, index);
            let end = payload_offset(&payload, index + 1);
            let decoded = decode_for_test(&blocks[start..end], atlas.bytes_per_glyph());
            assert_eq!(decoded, packed_cell(cell, width), "glyph {index}");
        }
    }

    #[test]
    fn ranges_are_sorted_dense_and_non_overlapping() {
        let atlas = committed_atlas();
        let mut previous_end = 0u32;
        let mut expected_base = 0u32;
        for range in &atlas.ranges {
            assert!(range.len > 0);
            assert!(
                range.first >= previous_end,
                "ranges overlap or are unsorted"
            );
            assert_eq!(range.base, expected_base, "cells are not in range order");
            previous_end = range.first + range.len;
            expected_base += range.len;
        }
        assert_eq!(expected_base as usize, atlas.cells.len());
    }

    #[test]
    fn printable_ascii_is_covered_and_space_is_blank() {
        let atlas = committed_atlas();
        for code in 0x20..=0x7Eu32 {
            assert!(
                atlas
                    .ranges
                    .iter()
                    .any(|r| (r.first..r.first + r.len).contains(&code)),
                "U+{code:04X} is not covered"
            );
        }
        assert!(
            cell_of(&atlas, 0x20).iter().all(|&c| c == 0),
            "space is not blank"
        );
        assert!(
            cell_of(&atlas, u32::from('A')).contains(&15),
            "'A' has no fully covered pixel"
        );
    }

    #[test]
    fn the_tiling_ranges_are_drawn_to_the_pixel_grid() {
        let atlas = committed_atlas();
        let width = atlas.geometry.width as usize;
        let glyph_width = width * MAX_GLYPH_CELLS as usize;
        // U+2588 FULL BLOCK covers every pixel of its own cell and none of the
        // continuation cell, so a filled region shows no seam at a cell edge.
        for (index, &coverage) in cell_of(&atlas, 0x2588).iter().enumerate() {
            let expected = if index % glyph_width < width { 15 } else { 0 };
            assert_eq!(coverage, expected, "full block pixel {index}");
        }
        // Every scalar of both ranges is whole pixels: a partly covered one is
        // the antialiased haze synthesising them exists to avoid.
        for code in 0x2500..0x25A0 {
            for &coverage in cell_of(&atlas, code) {
                let shade = matches!(code, 0x2591..=0x2593);
                assert!(
                    shade || coverage == 0 || coverage == 15,
                    "U+{code:04X} is partly covered"
                );
            }
        }
    }

    #[test]
    fn combining_mark_is_rendered_as_a_spacing_glyph() {
        let atlas = committed_atlas();
        // U+0301 COMBINING ACUTE ACCENT: zero advance, but the face draws
        // its outline inside the advance-wide cell, so it must have visible
        // ink.
        assert!(
            cell_of(&atlas, 0x0301).iter().any(|&c| c > 0),
            "combining acute has no ink"
        );
    }

    #[test]
    fn cyrillic_is_covered_with_ink() {
        let atlas = committed_atlas();
        // The whole Cyrillic block the face maps must rasterise with real
        // ink, not fall back to U+FFFD — the Ukrainian console regression:
        // і ї є ґ plus the base alphabet.
        for ch in [
            'і', 'ї', 'є', 'ґ', 'І', 'Ї', 'Є', 'Ґ', 'а', 'я', 'А', 'Я', 'Щ', 'ь',
        ] {
            let cell = cell_of(&atlas, u32::from(ch));
            assert!(cell.iter().any(|&c| c > 0), "{ch:?} has no ink");
        }
    }

    #[test]
    fn every_companion_face_is_compiled_in_with_ink() {
        let atlas = committed_atlas();
        // The kernel text console cannot ask `fontd` for a glyph, so every
        // script the family ships has to be here or the console can never draw
        // it. Coverage alone is not enough: a mapped-but-blank cell would draw
        // nothing at all, which reads as text that silently vanished.
        for ch in [
            'あ', 'ア', '漢', '字', '日', '本', '語', '가', '각', '한', '글', 'א', 'ב', 'ה', 'ש',
        ] {
            let cell = cell_of(&atlas, u32::from(ch));
            assert!(cell.iter().any(|&c| c > 0), "{ch:?} has no ink");
        }
    }

    #[test]
    fn a_full_width_scalar_is_drawn_across_both_cells() {
        let atlas = committed_atlas();
        // A wide scalar occupies two terminal cells; ink confined to the first
        // would leave the console drawing half a character.
        let width = atlas.geometry.width as usize;
        for ch in ['日', '語', '한'] {
            let cell = cell_of(&atlas, u32::from(ch));
            let inked = |column: usize| {
                cell.chunks(width * MAX_GLYPH_CELLS as usize)
                    .any(|row| row[column] > 0)
            };
            assert!((0..width).any(inked), "{ch:?} lead cell is empty");
            assert!(
                (width..width * MAX_GLYPH_CELLS as usize).any(inked),
                "{ch:?} continuation cell is empty"
            );
        }
    }

    #[test]
    fn fallback_is_the_replacement_character() {
        let atlas = committed_atlas();
        let cell = cell_of(&atlas, 0xFFFD);
        assert!(cell.iter().any(|&c| c > 0), "U+FFFD has no ink");
        let range = atlas
            .ranges
            .iter()
            .find(|r| (r.first..r.first + r.len).contains(&0xFFFD))
            .expect("U+FFFD mapped");
        assert_eq!(range.base + (0xFFFD - range.first), atlas.fallback);
    }

    #[test]
    fn generation_is_deterministic() {
        let family = committed_family();
        let a = build_atlas(&family.face_bytes()).expect("atlas builds");
        let b = build_atlas(&family.face_bytes()).expect("atlas builds");
        assert_eq!(a.render_rust(), b.render_rust());
        assert_eq!(
            a.coverage_bytes().expect("first encoding succeeds"),
            b.coverage_bytes().expect("second encoding succeeds")
        );
    }

    #[test]
    fn truncated_face_fails_closed() {
        let family = committed_family();
        let face = family.face_bytes()[0];
        assert!(build_atlas(&[]).is_err());
        assert!(build_atlas(&[&face[..64]]).is_err());
        assert!(build_atlas(&[&face[..face.len() / 2]]).is_err());
    }
}
