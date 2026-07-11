//! Decoding and pixel assertions for QEMU monitor `screendump` images.
//!
//! The HMP `screendump <path>` command writes the guest's current display
//! surface as a **binary PPM** (`P6`, 8-bit samples): a textual header
//! (magic, width, height, maximum sample value, each separated by
//! whitespace and optional `#` comments) followed by exactly
//! `width * height * 3` RGB bytes. [`parse_ppm`] decodes that format
//! fail-closed — a truncated, oversized, or malformed file is an error,
//! never a partial image — which is also how the runner distinguishes a
//! dump QEMU is still writing from a complete one (`Spec::screendump`
//! holds the pointer injection back until the file parses).
//!
//! [`Image::dominant_color`] is the assertion primitive a display
//! vertical uses: the single most frequent pixel colour and its share of
//! the surface. A composited desktop is dominated by its background
//! colour whatever the taskbar, cursor, or window chrome overlays, so
//! the check is robust against everything except the frame simply not
//! having been presented.

use std::collections::HashMap;

/// Upper bound on the pixel count of an accepted dump
/// (`width * height`) — a validation bound on the (host-produced, but
/// still parsed fail-closed) input, comfortably above any scan-out mode
/// the verticals emulate (16384 × 16384).
const MAX_PIXELS: u64 = 16_384 * 16_384;

/// A decoded RGB image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
    /// Width in pixels (non-zero).
    pub width: u32,
    /// Height in pixels (non-zero).
    pub height: u32,
    /// Row-major RGB samples, exactly `width * height * 3` bytes.
    pub pixels: Vec<u8>,
}

impl Image {
    /// The pixel at `(x, y)` as `(r, g, b)`.
    ///
    /// # Errors
    ///
    /// A message naming the out-of-bounds coordinate.
    pub fn pixel(&self, x: u32, y: u32) -> Result<(u8, u8, u8), String> {
        if x >= self.width || y >= self.height {
            return Err(format!(
                "pixel ({x}, {y}) out of bounds for {}x{}",
                self.width, self.height
            ));
        }
        let index = (y as usize * self.width as usize + x as usize) * 3;
        Ok((
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
        ))
    }

    /// The single most frequent colour and the fraction of all pixels it
    /// covers (`0.0..=1.0`).
    ///
    /// Ties resolve to the smallest colour value, so the answer is
    /// deterministic. The image is never empty ([`parse_ppm`] refuses
    /// zero dimensions), so a dominant colour always exists.
    #[must_use]
    pub fn dominant_color(&self) -> ((u8, u8, u8), f64) {
        let mut counts: HashMap<(u8, u8, u8), u64> = HashMap::new();
        for rgb in self.pixels.chunks_exact(3) {
            *counts.entry((rgb[0], rgb[1], rgb[2])).or_insert(0) += 1;
        }
        let total = (self.pixels.len() / 3) as u64;
        let (color, count) = counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .unwrap_or(((0, 0, 0), 0));
        #[allow(clippy::cast_precision_loss)] // Pixel counts are far below 2^52.
        let share = if total == 0 {
            0.0
        } else {
            count as f64 / total as f64
        };
        (color, share)
    }
}

/// Decode a binary PPM (`P6`, 8-bit) image, fail-closed.
///
/// # Errors
///
/// A message naming the defect: wrong magic, malformed or missing header
/// fields, an unsupported maximum sample value, zero or absurd
/// dimensions, or a byte count that does not match the header exactly
/// (both truncation and trailing garbage are refused).
pub fn parse_ppm(bytes: &[u8]) -> Result<Image, String> {
    let mut cursor = 0usize;
    let magic = read_token(bytes, &mut cursor).ok_or("missing PPM magic")?;
    if magic != b"P6" {
        return Err(format!(
            "not a binary PPM: magic {:?}",
            String::from_utf8_lossy(magic)
        ));
    }
    let width: u32 = parse_number(bytes, &mut cursor, "width")?;
    let height: u32 = parse_number(bytes, &mut cursor, "height")?;
    let maxval: u32 = parse_number(bytes, &mut cursor, "maxval")?;
    if width == 0 || height == 0 {
        return Err(format!("zero-sized image: {width}x{height}"));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("image too large: {width}x{height}"));
    }
    if maxval != 255 {
        return Err(format!("unsupported maxval {maxval} (only 8-bit samples)"));
    }
    // Exactly one whitespace byte separates the header from the samples.
    match bytes.get(cursor) {
        Some(b) if b.is_ascii_whitespace() => cursor += 1,
        _ => return Err("missing header/sample separator".into()),
    }
    let expected = width as usize * height as usize * 3;
    let samples = &bytes[cursor..];
    if samples.len() != expected {
        return Err(format!(
            "sample byte count {} does not match header ({expected} expected)",
            samples.len()
        ));
    }
    Ok(Image {
        width,
        height,
        pixels: samples.to_vec(),
    })
}

/// Read the next whitespace-delimited header token, skipping `#` comment
/// lines, advancing `cursor` past it. `None` at end of input.
fn read_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    loop {
        // Skip whitespace.
        while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
        // Skip a comment to end of line and retry.
        if bytes.get(*cursor) == Some(&b'#') {
            while let Some(b) = bytes.get(*cursor) {
                *cursor += 1;
                if *b == b'\n' {
                    break;
                }
            }
            continue;
        }
        break;
    }
    let start = *cursor;
    while bytes.get(*cursor).is_some_and(|b| !b.is_ascii_whitespace()) {
        *cursor += 1;
    }
    (*cursor > start).then(|| &bytes[start..*cursor])
}

/// Parse the next header token as a decimal `u32` named `what`.
fn parse_number(bytes: &[u8], cursor: &mut usize, what: &str) -> Result<u32, String> {
    let token = read_token(bytes, cursor).ok_or_else(|| format!("missing PPM {what}"))?;
    std::str::from_utf8(token)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed PPM {what}: {:?}", String::from_utf8_lossy(token)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed 2×2 P6 image with three red pixels and one blue.
    fn sample() -> Vec<u8> {
        let mut bytes = b"P6\n# a comment\n2 2\n255\n".to_vec();
        bytes.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0, 255]);
        bytes
    }

    #[test]
    fn parses_a_well_formed_image() {
        let image = parse_ppm(&sample()).expect("well-formed image parses");
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.pixel(0, 0), Ok((255, 0, 0)));
        assert_eq!(image.pixel(1, 1), Ok((0, 0, 255)));
        assert!(image.pixel(2, 0).is_err(), "out of bounds is refused");
    }

    #[test]
    fn dominant_color_reports_the_majority_and_share() {
        let image = parse_ppm(&sample()).expect("well-formed image parses");
        let (color, share) = image.dominant_color();
        assert_eq!(color, (255, 0, 0));
        assert!((share - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn truncated_samples_are_refused() {
        // A dump QEMU is still writing: header present, samples short.
        let mut bytes = sample();
        bytes.pop();
        assert!(parse_ppm(&bytes).is_err());
    }

    #[test]
    fn trailing_garbage_is_refused() {
        let mut bytes = sample();
        bytes.push(0);
        assert!(parse_ppm(&bytes).is_err());
    }

    #[test]
    fn wrong_magic_zero_size_and_wide_samples_are_refused() {
        assert!(parse_ppm(b"P5\n2 2\n255\n").is_err(), "not P6");
        assert!(parse_ppm(b"P6\n0 2\n255\n").is_err(), "zero width");
        assert!(parse_ppm(b"P6\n2 2\n65535\n").is_err(), "16-bit samples");
        assert!(parse_ppm(b"").is_err(), "empty input");
    }

    #[test]
    fn oversized_dimensions_are_refused() {
        assert!(parse_ppm(b"P6\n1000000 1000000\n255\n").is_err());
    }
}
