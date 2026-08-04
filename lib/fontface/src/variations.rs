//! OpenType Font Variations: instancing a `glyf` face at a chosen point in its
//! design space.
//!
//! A variable face declares variation *axes* (`fvar`), an optional axis
//! remapping (`avar`), per-glyph outline deltas (`gvar`), and per-glyph advance
//! deltas (`HVAR`). Choosing a user-space setting (a weight, a width, …)
//! normalises to a coordinate on each axis; the outline and advance are then
//! the base values plus the sum of every applicable delta scaled by how
//! strongly the current coordinate falls inside each delta's region.
//!
//! The engine resolves the coordinate once, at parse time, and applies deltas
//! lazily — only when a glyph is actually measured or rasterised — so a
//! 17 MB CJK face costs nothing per un-drawn glyph.
//!
//! Every read goes through the bounds-checked [`Reader`]; a truncated or
//! self-inconsistent table fails closed with a [`FontError`], and every count
//! taken from the file is bounded by the glyph's own point count so a hostile
//! face cannot drive an unbounded allocation or loop.

use alloc::vec;
use alloc::vec::Vec;

use crate::engine::{err, Reader};
use crate::mathf;
use crate::FontError;

/// The data a simple glyph needs for IUP: its contour end indices and the
/// original (pre-delta) coordinate of every outline point.
type IupData<'c> = (&'c [u16], &'c [(f64, f64)]);

/// A tuple's intermediate region as its per-axis start and end coordinates.
type Region = (Vec<f64>, Vec<f64>);

/// One variation axis a face declares in its `fvar` table.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Axis {
    /// The four-byte axis tag (`wght`, `wdth`, `opsz`, …).
    pub tag: [u8; 4],
    /// The smallest user-space value the axis accepts.
    pub min: f32,
    /// The user-space value the face renders at when the axis is left unset.
    pub default: f32,
    /// The largest user-space value the axis accepts.
    pub max: f32,
}

/// A user-space coordinate requested for one axis when instancing a face.
///
/// A setting naming an axis the face does not declare is ignored; an axis no
/// setting names stays at its [`Axis::default`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AxisSetting {
    /// The four-byte tag of the axis this setting drives.
    pub tag: [u8; 4],
    /// The requested user-space value; clamped into the axis range.
    pub value: f32,
}

/// The variation tables of an instanced face, kept alongside the resolved
/// normalised coordinates so a glyph can be varied on demand.
pub(crate) struct VarTables {
    pub(crate) gvar: Gvar,
    pub(crate) hvar: Option<usize>,
}

/// Parse the `fvar` axis records at `fvar` into the declared axes.
///
/// # Errors
///
/// A [`FontError`] when a field is truncated, the axis-record stride is under
/// the 20-byte minimum, or an axis range is not `min <= default <= max`.
pub(crate) fn parse_fvar(r: &Reader<'_>, fvar: usize) -> Result<Vec<Axis>, FontError> {
    let axes_offset = fvar + usize::from(r.u16(fvar + 4)?);
    let axis_count = usize::from(r.u16(fvar + 8)?);
    let axis_size = usize::from(r.u16(fvar + 10)?);
    if axis_size < 20 {
        return Err(err("fvar axis record smaller than the spec minimum"));
    }
    let mut axes = Vec::with_capacity(axis_count);
    for i in 0..axis_count {
        let rec = axes_offset + i * axis_size;
        let tag = [r.u8(rec)?, r.u8(rec + 1)?, r.u8(rec + 2)?, r.u8(rec + 3)?];
        let min = fixed_to_f32(r.i32(rec + 4)?);
        let default = fixed_to_f32(r.i32(rec + 8)?);
        let max = fixed_to_f32(r.i32(rec + 12)?);
        if !(min <= default && default <= max) {
            return Err(err("fvar axis range is not min <= default <= max"));
        }
        axes.push(Axis {
            tag,
            min,
            default,
            max,
        });
    }
    Ok(axes)
}

/// The normalised coordinate for `axis` given the requested `settings`.
///
/// Resolves the user-space value (a matching setting, else the default),
/// clamps it into the axis range, and maps it to `-1..0..+1` against the
/// axis's min/default/max.
pub(crate) fn normalise_axis(axis: &Axis, settings: &[AxisSetting]) -> f32 {
    let user = settings
        .iter()
        .find(|s| s.tag == axis.tag)
        .map_or(axis.default, |s| s.value);
    let user = user.clamp(axis.min, axis.max);
    if user < axis.default {
        if axis.default > axis.min {
            -((axis.default - user) / (axis.default - axis.min))
        } else {
            0.0
        }
    } else if user > axis.default {
        if axis.max > axis.default {
            (user - axis.default) / (axis.max - axis.default)
        } else {
            0.0
        }
    } else {
        0.0
    }
}

/// Remap the normalised `coords` in place through the `avar` segment maps.
///
/// # Errors
///
/// A [`FontError`] when a field is truncated or the table's axis count differs
/// from the face's.
pub(crate) fn apply_avar(r: &Reader<'_>, avar: usize, coords: &mut [f32]) -> Result<(), FontError> {
    if usize::from(r.u16(avar + 6)?) != coords.len() {
        return Err(err("avar axis count differs from fvar"));
    }
    let mut at = avar + 8;
    for coord in coords.iter_mut() {
        let position_count = usize::from(r.u16(at)?);
        at += 2;
        let mut maps = Vec::with_capacity(position_count);
        for _ in 0..position_count {
            let from = f2dot14(r.i16(at)?);
            let to = f2dot14(r.i16(at + 2)?);
            at += 4;
            maps.push((from, to));
        }
        *coord = apply_segment_map(*coord, &maps);
    }
    Ok(())
}

/// Piecewise-linear remap of one normalised coordinate through a segment map.
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "the interpolation is over F2Dot14 endpoints in -1..=1, exactly \
              representable in f32, so the f64->f32 narrowing loses nothing; \
              the equality test guards a genuine zero-length segment"
)]
fn apply_segment_map(value: f32, maps: &[(f64, f64)]) -> f32 {
    let Some(&(first_from, first_to)) = maps.first() else {
        return value;
    };
    let x = f64::from(value);
    if x <= first_from {
        return first_to as f32;
    }
    for pair in maps.windows(2) {
        let (from_a, to_a) = pair[0];
        let (from_b, to_b) = pair[1];
        if x <= from_b {
            if from_b == from_a {
                return to_a as f32;
            }
            let t = (x - from_a) / (from_b - from_a);
            return (to_a + t * (to_b - to_a)) as f32;
        }
    }
    maps[maps.len() - 1].1 as f32
}

/// The parsed `gvar` header: where the per-glyph tuple variation stores live
/// and how to index them.
pub(crate) struct Gvar {
    axis_count: usize,
    shared_tuples: usize,
    shared_tuple_count: usize,
    glyph_count: usize,
    long_offsets: bool,
    offsets: usize,
    data_array: usize,
}

impl Gvar {
    /// Parse the `gvar` header at `gvar`, checking its axis count matches the
    /// face's `axis_count`.
    ///
    /// # Errors
    ///
    /// A [`FontError`] when a field is truncated or the axis count disagrees
    /// with `fvar`.
    pub(crate) fn parse(r: &Reader<'_>, gvar: usize, axis_count: usize) -> Result<Self, FontError> {
        if usize::from(r.u16(gvar + 4)?) != axis_count {
            return Err(err("gvar axis count differs from fvar"));
        }
        let shared_tuple_count = usize::from(r.u16(gvar + 6)?);
        let shared_tuples = gvar + offset32(r, gvar + 8)?;
        let glyph_count = usize::from(r.u16(gvar + 12)?);
        let long_offsets = r.u16(gvar + 14)? & 1 != 0;
        let data_array = gvar + offset32(r, gvar + 16)?;
        Ok(Self {
            axis_count,
            shared_tuples,
            shared_tuple_count,
            glyph_count,
            long_offsets,
            offsets: gvar + 20,
            data_array,
        })
    }

    /// The byte range of `glyph`'s variation data, or `None` when the glyph
    /// carries none.
    fn glyph_data(&self, r: &Reader<'_>, glyph: u16) -> Result<Option<(usize, usize)>, FontError> {
        let i = usize::from(glyph);
        if i >= self.glyph_count {
            return Err(err("gvar glyph id out of range"));
        }
        let (start, end) = if self.long_offsets {
            (
                offset32(r, self.offsets + 4 * i)?,
                offset32(r, self.offsets + 4 * i + 4)?,
            )
        } else {
            (
                usize::from(r.u16(self.offsets + 2 * i)?) * 2,
                usize::from(r.u16(self.offsets + 2 * i + 2)?) * 2,
            )
        };
        if end < start {
            return Err(err("gvar glyph offsets not monotonic"));
        }
        Ok((start != end).then_some((self.data_array + start, self.data_array + end)))
    }

    /// Accumulate `glyph`'s variation deltas for the current `coords` into
    /// `out`, one `(dx, dy)` per point (`n` outline points then the four
    /// phantom points). When `iup` is given the untouched points of a simple
    /// glyph are interpolated; otherwise (a composite, or an advance-only
    /// measurement) untouched points stay zero.
    ///
    /// Returns whether any tuple applied. `out` and the coordinate count are
    /// validated against `n`, so an inconsistent call fails closed.
    ///
    /// # Errors
    ///
    /// A [`FontError`] on any truncated or self-inconsistent tuple store.
    pub(crate) fn deltas(
        &self,
        r: &Reader<'_>,
        coords: &[f32],
        glyph: u16,
        n: usize,
        iup: Option<IupData<'_>>,
        out: &mut [(f64, f64)],
    ) -> Result<bool, FontError> {
        let total = n + 4;
        if out.len() != total || coords.len() != self.axis_count {
            return Err(err("gvar delta buffer or coordinate count mismatch"));
        }
        let Some((data, data_end)) = self.glyph_data(r, glyph)? else {
            return Ok(false);
        };
        let header_word = r.u16(data)?;
        let tuple_count = usize::from(header_word & 0x0FFF);
        if tuple_count == 0 {
            return Ok(false);
        }
        let mut serial = data
            .checked_add(usize::from(r.u16(data + 2)?))
            .ok_or(err("gvar serialized-data offset overflow"))?;
        if serial > data_end {
            return Err(err("gvar serialized-data offset past glyph data"));
        }
        let shared = if header_word & 0x8000 != 0 {
            let mut cur = ByteCursor::new(r, serial, data_end);
            let points = read_points(&mut cur, total)?;
            serial = cur.at;
            Some(points)
        } else {
            None
        };
        let mut acc = Accum {
            total,
            iup,
            out,
            tuple: vec![(0.0, 0.0); total],
            touched: vec![false; total],
        };
        let mut header = data + 4;
        let mut applied = false;
        for _ in 0..tuple_count {
            let var_size = usize::from(r.u16(header)?);
            let tuple_index = r.u16(header + 2)?;
            header += 4;
            let peak = self.read_peak(r, &mut header, tuple_index)?;
            let region = self.read_intermediate(r, &mut header, tuple_index)?;
            let end = serial
                .checked_add(var_size)
                .ok_or(err("gvar tuple size overflow"))?;
            if end > data_end {
                return Err(err("gvar tuple data overruns glyph data"));
            }
            let scalar = tuple_scalar(coords, &peak, region.as_ref());
            if scalar != 0.0 {
                let mut cur = ByteCursor::new(r, serial, end);
                let private;
                let points = if tuple_index & 0x2000 != 0 {
                    private = read_points(&mut cur, total)?;
                    &private
                } else {
                    shared
                        .as_ref()
                        .ok_or(err("gvar tuple references absent shared points"))?
                };
                let count = points.len(total);
                let xs = read_deltas(&mut cur, count)?;
                let ys = read_deltas(&mut cur, count)?;
                acc.add(points, &xs, &ys, scalar)?;
                applied = true;
            }
            serial = end;
        }
        Ok(applied)
    }

    /// Read a tuple's peak coordinate: the embedded peak, or the shared tuple
    /// its index selects.
    fn read_peak(
        &self,
        r: &Reader<'_>,
        header: &mut usize,
        tuple_index: u16,
    ) -> Result<Vec<f64>, FontError> {
        if tuple_index & 0x8000 != 0 {
            let peak = read_tuple(r, *header, self.axis_count)?;
            *header += 2 * self.axis_count;
            return Ok(peak);
        }
        let index = usize::from(tuple_index & 0x0FFF);
        if index >= self.shared_tuple_count {
            return Err(err("gvar shared tuple index out of range"));
        }
        read_tuple(
            r,
            self.shared_tuples + index * 2 * self.axis_count,
            self.axis_count,
        )
    }

    /// Read a tuple's optional intermediate start/end region.
    fn read_intermediate(
        &self,
        r: &Reader<'_>,
        header: &mut usize,
        tuple_index: u16,
    ) -> Result<Option<Region>, FontError> {
        if tuple_index & 0x4000 == 0 {
            return Ok(None);
        }
        let start = read_tuple(r, *header, self.axis_count)?;
        *header += 2 * self.axis_count;
        let end = read_tuple(r, *header, self.axis_count)?;
        *header += 2 * self.axis_count;
        Ok(Some((start, end)))
    }
}

/// Per-glyph delta accumulation: the running total plus the scratch a single
/// tuple is expanded into before it is scaled and added.
struct Accum<'c> {
    total: usize,
    iup: Option<IupData<'c>>,
    out: &'c mut [(f64, f64)],
    tuple: Vec<(f64, f64)>,
    touched: Vec<bool>,
}

impl Accum<'_> {
    /// Expand one tuple's `points`/`xs`/`ys` into per-point deltas
    /// (interpolating untouched points when this is a simple glyph), then add
    /// them to the running total scaled by `scalar`.
    fn add(
        &mut self,
        points: &PointSet,
        xs: &[i32],
        ys: &[i32],
        scalar: f64,
    ) -> Result<(), FontError> {
        self.tuple.iter_mut().for_each(|d| *d = (0.0, 0.0));
        self.touched.iter_mut().for_each(|t| *t = false);
        match points {
            PointSet::All => {
                for i in 0..self.total {
                    self.tuple[i] = (f64::from(xs[i]), f64::from(ys[i]));
                    self.touched[i] = true;
                }
            }
            PointSet::Explicit(indices) => {
                for (k, &point) in indices.iter().enumerate() {
                    let idx = usize::try_from(point).unwrap_or(usize::MAX);
                    if idx >= self.total {
                        return Err(err("gvar point number out of range"));
                    }
                    self.tuple[idx] = (f64::from(xs[k]), f64::from(ys[k]));
                    self.touched[idx] = true;
                }
                if let Some((ends, orig)) = self.iup {
                    interpolate_untouched(ends, orig, &self.touched, &mut self.tuple)?;
                }
            }
        }
        for (slot, delta) in self.out.iter_mut().zip(self.tuple.iter()) {
            slot.0 += scalar * delta.0;
            slot.1 += scalar * delta.1;
        }
        Ok(())
    }
}

/// The set of points a tuple supplies deltas for.
enum PointSet {
    /// Every point of the glyph, in order.
    All,
    /// A specific, ascending list of point numbers.
    Explicit(Vec<u32>),
}

impl PointSet {
    /// How many delta values this set carries for a glyph with `total` points.
    fn len(&self, total: usize) -> usize {
        match self {
            Self::All => total,
            Self::Explicit(indices) => indices.len(),
        }
    }
}

/// Interpolate the untouched points of each contour (IUP) from their touched
/// neighbours, per axis, with the standard cyclic wrap-around.
///
/// # Errors
///
/// A [`FontError`] when a contour end index is out of range.
fn interpolate_untouched(
    ends: &[u16],
    orig: &[(f64, f64)],
    touched: &[bool],
    tuple: &mut [(f64, f64)],
) -> Result<(), FontError> {
    let n = orig.len();
    let mut first = 0usize;
    for &end in ends {
        let last = usize::from(end);
        if last < first || last >= n {
            return Err(err("gvar contour end out of range for IUP"));
        }
        iup_contour(
            &orig[first..=last],
            &touched[first..=last],
            &mut tuple[first..=last],
        );
        first = last + 1;
    }
    Ok(())
}

/// Interpolate one contour's untouched points between their nearest touched
/// neighbours (cyclically). A contour with no touched point is left at zero;
/// with one, every point takes that point's delta.
fn iup_contour(orig: &[(f64, f64)], touched: &[bool], delta: &mut [(f64, f64)]) {
    let n = orig.len();
    let touched_indices: Vec<usize> = (0..n).filter(|&i| touched[i]).collect();
    if touched_indices.is_empty() {
        return;
    }
    for w in 0..touched_indices.len() {
        let a = touched_indices[w];
        let b = touched_indices[(w + 1) % touched_indices.len()];
        let mut j = (a + 1) % n;
        while j != b {
            delta[j].0 = interpolate(orig[j].0, orig[a].0, delta[a].0, orig[b].0, delta[b].0);
            delta[j].1 = interpolate(orig[j].1, orig[a].1, delta[a].1, orig[b].1, delta[b].1);
            j = (j + 1) % n;
        }
    }
}

/// One-axis IUP interpolation: the delta at coordinate `c` between two touched
/// reference points `(c1, d1)` and `(c2, d2)`.
fn interpolate(c: f64, mut c1: f64, mut d1: f64, mut c2: f64, mut d2: f64) -> f64 {
    if c1 > c2 {
        core::mem::swap(&mut c1, &mut c2);
        core::mem::swap(&mut d1, &mut d2);
    }
    if c1 >= c2 {
        // The two references coincide; only a matching delta is well defined,
        // so a disagreement contributes no shift.
        return if d1.total_cmp(&d2) == core::cmp::Ordering::Equal {
            d1
        } else {
            0.0
        };
    }
    if c <= c1 {
        d1
    } else if c >= c2 {
        d2
    } else {
        d1 + (c - c1) / (c2 - c1) * (d2 - d1)
    }
}

/// The combined scalar for a tuple: the product over axes of how strongly the
/// current `coords` fall inside the tuple's `peak` (and optional intermediate
/// `region`).
fn tuple_scalar(coords: &[f32], peak: &[f64], region: Option<&Region>) -> f64 {
    let mut scalar = 1.0;
    for (axis, &p) in peak.iter().enumerate() {
        let (lower, upper) = match region {
            Some((start, end)) => (start[axis], end[axis]),
            None => (mathf::fmin(p, 0.0), mathf::fmax(p, 0.0)),
        };
        scalar *= axis_factor(f64::from(coords[axis]), lower, p, upper);
        if scalar == 0.0 {
            break;
        }
    }
    scalar
}

/// How strongly coordinate `v` falls inside one axis region `[lower, peak,
/// upper]`. Returns `1.0` for an axis the region does not restrict, `0.0`
/// outside the region, and the linear ramp toward the peak inside it.
#[allow(
    clippy::float_cmp,
    reason = "these are exact comparisons against decoded F2Dot14 endpoints \
              and the peak, matching the OpenType support-scalar definition; a \
              tolerance would change which deltas apply"
)]
fn axis_factor(v: f64, lower: f64, peak: f64, upper: f64) -> f64 {
    if peak == 0.0 || lower > peak || peak > upper || (lower < 0.0 && upper > 0.0) {
        return 1.0;
    }
    if v == peak {
        return 1.0;
    }
    if v <= lower || upper <= v {
        return 0.0;
    }
    if v < peak {
        (v - lower) / (peak - lower)
    } else {
        (upper - v) / (upper - peak)
    }
}

/// The advance-width delta `HVAR` supplies for `glyph` at `coords`, in font
/// units, already rounded to the nearest integer.
///
/// # Errors
///
/// A [`FontError`] on any truncated or self-inconsistent `HVAR` structure.
pub(crate) fn hvar_advance_delta(
    r: &Reader<'_>,
    hvar: usize,
    coords: &[f32],
    glyph: u16,
) -> Result<i32, FontError> {
    let store = hvar + offset32(r, hvar + 4)?;
    let map_offset = r.u32(hvar + 8)?;
    let (outer, inner) = if map_offset == 0 {
        (0, u32::from(glyph))
    } else {
        delta_set_index(
            r,
            hvar + usize::try_from(map_offset).unwrap_or(usize::MAX),
            glyph,
        )?
    };
    Ok(mathf::round_i32(item_variation_delta(
        r, store, coords, outer, inner,
    )?))
}

/// Look `glyph` up in a `DeltaSetIndexMap`, returning its `(outer, inner)`
/// item-variation-store index.
fn delta_set_index(r: &Reader<'_>, map: usize, glyph: u16) -> Result<(u32, u32), FontError> {
    let format = r.u8(map)?;
    let entry_format = r.u8(map + 1)?;
    let (map_count, entries) = match format {
        0 => (u32::from(r.u16(map + 2)?), map + 4),
        1 => (r.u32(map + 2)?, map + 6),
        _ => return Err(err("unknown DeltaSetIndexMap format")),
    };
    let entry_size = usize::from((entry_format >> 4) & 0x3) + 1;
    let inner_bits = u32::from(entry_format & 0x0F) + 1;
    let index = if u32::from(glyph) < map_count {
        usize::from(glyph)
    } else {
        usize::try_from(
            map_count
                .checked_sub(1)
                .ok_or(err("empty DeltaSetIndexMap"))?,
        )
        .unwrap_or(usize::MAX)
    };
    let mut value = 0u32;
    for k in 0..entry_size {
        value = (value << 8) | u32::from(r.u8(entries + entry_size * index + k)?);
    }
    let inner_mask = (1u32 << inner_bits) - 1;
    Ok((value >> inner_bits, value & inner_mask))
}

/// The delta an `ItemVariationStore` yields for item `(outer, inner)` at
/// `coords`.
fn item_variation_delta(
    r: &Reader<'_>,
    store: usize,
    coords: &[f32],
    outer: u32,
    inner: u32,
) -> Result<f64, FontError> {
    let region_list = store + offset32(r, store + 2)?;
    let data_count = usize::from(r.u16(store + 6)?);
    let outer = usize::try_from(outer).unwrap_or(usize::MAX);
    if outer >= data_count {
        return Err(err("HVAR outer index out of range"));
    }
    let data = store + offset32(r, store + 8 + 4 * outer)?;
    let axis_count = usize::from(r.u16(region_list)?);
    if axis_count != coords.len() {
        return Err(err("HVAR region axis count differs from fvar"));
    }
    let region_count = usize::from(r.u16(region_list + 2)?);
    let item_count = usize::from(r.u16(data)?);
    let word_field = r.u16(data + 2)?;
    let long_words = word_field & 0x8000 != 0;
    let word_count = usize::from(word_field & 0x7FFF);
    let region_index_count = usize::from(r.u16(data + 4)?);
    let inner = usize::try_from(inner).unwrap_or(usize::MAX);
    if word_count > region_index_count || inner >= item_count {
        return Err(err("HVAR item variation data is inconsistent"));
    }
    let (word_size, short_size) = if long_words { (4, 2) } else { (2, 1) };
    let row_size = word_count * word_size + (region_index_count - word_count) * short_size;
    let region_indexes = data + 6;
    let mut at = region_indexes + 2 * region_index_count + row_size * inner;
    let mut delta = 0.0;
    for k in 0..region_index_count {
        let region = usize::from(r.u16(region_indexes + 2 * k)?);
        if region >= region_count {
            return Err(err("HVAR region index out of range"));
        }
        let long = k < word_count;
        let raw = match (long, long_words) {
            (true, true) => r.i32(at)?,
            (true, false) | (false, true) => i32::from(r.i16(at)?),
            (false, false) => i32::from(r.i8(at)?),
        };
        at += if long { word_size } else { short_size };
        delta += f64::from(raw) * region_scalar(r, region_list + 4, axis_count, region, coords)?;
    }
    Ok(delta)
}

/// The scalar for one `ItemVariationStore` region at `coords`.
///
/// # Errors
///
/// A [`FontError`] when a region-axis coordinate is truncated.
fn region_scalar(
    r: &Reader<'_>,
    regions: usize,
    axis_count: usize,
    region: usize,
    coords: &[f32],
) -> Result<f64, FontError> {
    let base = regions + region * axis_count * 6;
    let mut scalar = 1.0;
    for (axis, &coord) in coords.iter().enumerate() {
        let at = base + axis * 6;
        let start = f2dot14(r.i16(at)?);
        let peak = f2dot14(r.i16(at + 2)?);
        let end = f2dot14(r.i16(at + 4)?);
        scalar *= axis_factor(f64::from(coord), start, peak, end);
        if scalar == 0.0 {
            break;
        }
    }
    Ok(scalar)
}

/// A forward byte cursor bounded to a sub-range, so a length a tuple claims
/// can never read past the region the header sized for it.
struct ByteCursor<'a, 'b> {
    r: &'b Reader<'a>,
    at: usize,
    limit: usize,
}

impl<'a, 'b> ByteCursor<'a, 'b> {
    fn new(r: &'b Reader<'a>, at: usize, limit: usize) -> Self {
        Self { r, at, limit }
    }

    fn take(&mut self, len: usize) -> Result<usize, FontError> {
        let next = self
            .at
            .checked_add(len)
            .ok_or(err("gvar cursor overflow"))?;
        if next > self.limit {
            return Err(err("gvar serialized data overruns its region"));
        }
        let at = self.at;
        self.at = next;
        Ok(at)
    }

    fn u8(&mut self) -> Result<u8, FontError> {
        self.r.u8(self.take(1)?)
    }

    fn i8(&mut self) -> Result<i8, FontError> {
        self.r.i8(self.take(1)?)
    }

    fn u16(&mut self) -> Result<u16, FontError> {
        self.r.u16(self.take(2)?)
    }

    fn i16(&mut self) -> Result<i16, FontError> {
        self.r.i16(self.take(2)?)
    }
}

/// Read a packed point-number set, bounded by the glyph's `total` point count.
///
/// A leading count of zero selects every point; otherwise the count is capped
/// at `total` (a hostile larger count fails closed) and read as run-length
/// encoded cumulative point numbers.
fn read_points(cur: &mut ByteCursor<'_, '_>, total: usize) -> Result<PointSet, FontError> {
    let first = cur.u8()?;
    let count = if first & 0x80 != 0 {
        (usize::from(first & 0x7F) << 8) | usize::from(cur.u8()?)
    } else {
        usize::from(first)
    };
    if count == 0 {
        return Ok(PointSet::All);
    }
    if count > total {
        return Err(err("gvar point count exceeds glyph point count"));
    }
    let mut points = Vec::with_capacity(count);
    let mut point = 0u32;
    while points.len() < count {
        let control = cur.u8()?;
        let words = control & 0x80 != 0;
        let run = usize::from(control & 0x7F) + 1;
        for _ in 0..run {
            if points.len() == count {
                break;
            }
            let step = if words {
                u32::from(cur.u16()?)
            } else {
                u32::from(cur.u8()?)
            };
            point = point
                .checked_add(step)
                .ok_or(err("gvar point number overflow"))?;
            points.push(point);
        }
    }
    Ok(PointSet::Explicit(points))
}

/// Read exactly `count` packed deltas (one coordinate axis of one tuple).
fn read_deltas(cur: &mut ByteCursor<'_, '_>, count: usize) -> Result<Vec<i32>, FontError> {
    let mut deltas = Vec::with_capacity(count);
    while deltas.len() < count {
        let control = cur.u8()?;
        let run = usize::from(control & 0x3F) + 1;
        if deltas.len() + run > count {
            return Err(err("gvar delta run overruns its point set"));
        }
        if control & 0x80 != 0 {
            deltas.resize(deltas.len() + run, 0);
        } else if control & 0x40 != 0 {
            for _ in 0..run {
                deltas.push(i32::from(cur.i16()?));
            }
        } else {
            for _ in 0..run {
                deltas.push(i32::from(cur.i8()?));
            }
        }
    }
    Ok(deltas)
}

/// Read `axis_count` `F2Dot14` coordinates into a tuple.
fn read_tuple(r: &Reader<'_>, at: usize, axis_count: usize) -> Result<Vec<f64>, FontError> {
    let mut tuple = Vec::with_capacity(axis_count);
    for axis in 0..axis_count {
        tuple.push(f2dot14(r.i16(at + 2 * axis)?));
    }
    Ok(tuple)
}

/// A 32-bit table offset read as a `usize`.
fn offset32(r: &Reader<'_>, at: usize) -> Result<usize, FontError> {
    usize::try_from(r.u32(at)?).map_err(|_| err("32-bit offset exceeds the address space"))
}

/// An `F2Dot14` fixed-point value as an `f64`.
fn f2dot14(raw: i16) -> f64 {
    f64::from(raw) / 16384.0
}

/// A 16.16 fixed-point value as an `f32`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "an fvar axis value is a small coordinate (hundreds at most), \
              exactly representable in f32, so the f64->f32 narrowing loses \
              nothing here"
)]
fn fixed_to_f32(raw: i32) -> f32 {
    (f64::from(raw) / 65536.0) as f32
}
