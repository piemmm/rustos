//! The shared separable box blur.
//!
//! One definition serves every frosted surface: the compositor's backdrop
//! blur (a window's rectangle frosted before its own translucent pixels
//! blend over it) and a control's own soft highlight. A horizontal pass then
//! a vertical one, each carrying a running sum, so the cost is the region's
//! area rather than its area times the radius.
//!
//! Every channel — alpha included — is averaged. Averaging premultiplied
//! channels is the correct operation on premultiplied data: the result is
//! the same convex combination of the contributing colours that compositing
//! them would give, so the `colour <= alpha` invariant survives and no halo
//! appears around a translucent edge.
//!
//! Edges replicate: a sample past the region's edge takes the edge pixel.
//! Every output therefore averages exactly `2 * radius + 1` samples, which
//! keeps the divisor constant across the pass and leaves a uniform field
//! exactly unchanged.

use core::ops::Range;

use alloc::vec::Vec;

use tairix_parallel::JobRunner;

use crate::color::{mix, Pixel};
use crate::dither::DitherRow;
use crate::surface::{RowBand, Surface};

/// Blur `region` in place: a dense, row-major, premultiplied
/// `width`×`height` block of pixels, blurred by a separable box blur of
/// `radius` physical pixels using `aux` as the intermediate buffer.
///
/// `aux` is supplied by the caller — so a blur costs no allocation on the
/// frame path. A caller frosting a rectangle of a surface takes
/// [`Surface::frost_region`] instead, which reads the surface itself and owns
/// the shape-weighted mix back into it, carrying both buffers in a
/// [`BlurScratch`].
///
/// Nothing is blurred, and `region` is left exactly as it was, when the
/// radius is `0` (the effect is disabled), when either dimension is `0`, or
/// when `region` or `aux` is shorter than `width * height`: a caller that
/// mis-sizes a buffer gets the unblurred backdrop, never a partly-blurred
/// or out-of-bounds one.
pub fn box_blur(
    region: &mut [Pixel],
    width: usize,
    height: usize,
    radius: usize,
    aux: &mut [Pixel],
) {
    let Some(count) = width.checked_mul(height) else {
        return;
    };
    if radius == 0 || count == 0 || region.len() < count || aux.len() < count {
        return;
    }
    // The window is the same size for every output of both passes, so its
    // divisor is resolved here rather than per pixel.
    let recip = Reciprocal::new(radius.saturating_mul(2).saturating_add(1));
    for y in 0..height {
        let Some((src, dst)) = row_pair(region, aux, y, width) else {
            return;
        };
        blur_span(src, dst, 1, width, radius, recip, 0..width);
    }
    for x in 0..width {
        let Some((src, dst)) = column_pair(aux, region, x, count) else {
            return;
        };
        blur_span(src, dst, width, height, radius, recip, 0..height);
    }
}

/// The fewest pixels a piece of a frost carries before it is worth handing to
/// another core.
///
/// A piece is two sliding-window passes plus a mix over its own pixels, which at
/// this size is hundreds of microseconds even on a slow core — several times what
/// a dispatch's wake and park syscalls cost. Below it the frost runs on the
/// calling thread with no atomics, so a small frosted control costs what it
/// always did.
const MIN_PARALLEL_FROST_PX: usize = 8_192;

/// The buffers a frost works in: the blurred pixels it mixes back, the
/// intermediate its horizontal pass hands to its vertical one, and the pieces it
/// splits itself into.
///
/// All three grow to the largest frost asked of them and are then reused, so a
/// per-frame caller — the compositor frosting a window's backdrop on every
/// composite — allocates nothing once its scratch is warm.
#[derive(Default)]
pub struct BlurScratch {
    frosted: Vec<Pixel>,
    aux: Vec<Pixel>,
    pieces: Vec<Band>,
}

impl BlurScratch {
    /// An empty scratch, which allocates on its first frost and not before.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frosted: Vec::new(),
            aux: Vec::new(),
            pieces: Vec::new(),
        }
    }

    /// Give the memory back, so a scratch that frosted a large region stops
    /// holding it; the next frost grows it again.
    pub fn release(&mut self) {
        self.frosted = Vec::new();
        self.aux = Vec::new();
        self.pieces = Vec::new();
    }
}

/// Lengthen `buffer` to `count` pixels, reserving first so exhaustion is
/// answered with `false` rather than an allocation abort.
fn grow(buffer: &mut Vec<Pixel>, count: usize) -> bool {
    let Some(extra) = count.checked_sub(buffer.len()) else {
        return true;
    };
    if buffer.try_reserve(extra).is_err() {
        return false;
    }
    buffer.resize(count, Pixel::TRANSPARENT);
    true
}

impl Surface {
    /// Frost `[x, x+w) × [y, y+h)`: blur the pixels already there by
    /// `radius` and mix the blurred copy back over them at each pixel's
    /// `coverage` — `255` takes the blurred pixel, `0` keeps the original,
    /// and the values between are the weighted mix.
    ///
    /// This is the desktop's one frosted glass: the compositor frosts a
    /// window's backdrop before the window's own translucent pixels blend
    /// over it, and the login screen frosts the wallpaper behind a selected
    /// account tile. Weighting the mix rather than clipping it is what lets
    /// a rounded shape fade from frosted to untouched across its own arc
    /// instead of showing a square edge.
    ///
    /// `coverage` is asked about a pixel's position relative to the
    /// rectangle's **own** top-left, so a caller whose rectangle the surface
    /// edge or the clip window cuts short still reads the whole shape rather
    /// than re-fitting it to what survives.
    ///
    /// A partial coverage is a translucent field over a picture, so the mix
    /// rounds through the surface's own ordered dither: a frosted bar over a
    /// smooth wallpaper keeps the wallpaper's gradient instead of stepping it
    /// into plateaus. Full coverage takes the blurred pixel exactly and no
    /// coverage leaves the destination exactly, at every bias.
    ///
    /// The frost is confined to what the surface bounds and the active clip
    /// window admit and reads only the pixels it may write: samples past
    /// that edge replicate it, so the effect can neither pull a neighbour's
    /// pixels in nor mark a pixel outside. `scratch` carries the blurred
    /// pixels and the pass-to-pass intermediate between calls and holds
    /// nothing from one frost to the next.
    ///
    /// The surface is left exactly as it was, never partly frosted, when
    /// `radius` is `0` (the effect is disabled), when the rectangle is empty
    /// or lands nowhere the surface admits, or when the scratch could not be
    /// grown.
    #[expect(
        clippy::too_many_arguments,
        reason = "the rectangle is spelled as the four scalars every other \
                  Surface primitive takes, plus the blur radius, the caller's \
                  reused scratch, where its pieces run, and the shape being \
                  frosted"
    )]
    pub fn frost_region(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        radius: u32,
        scratch: &mut BlurScratch,
        runner: &dyn JobRunner,
        coverage: impl Fn(u32, u32) -> u8 + Sync,
    ) {
        self.frost(x, y, w, h, None, radius, scratch, runner, coverage);
    }

    /// Frost `[x, x+w) × [y, y+h)` **around** `keep_cols` × `keep_rows` — the
    /// columns and rows given relative to that rectangle's own top-left —
    /// writing exactly the pixels [`frost_region`](Self::frost_region) would
    /// write outside the kept block and leaving the block itself untouched.
    ///
    /// The whole rectangle still decides the answer: samples replicate at
    /// *its* edges, and coverage is read at its own coordinates, so this is
    /// never a smaller frost of a smaller rectangle — which would spread a
    /// neighbourhood clipped to the border and seam against the kept pixels.
    /// What it buys is cost: each pass runs over the border and `radius`
    /// beyond it rather than over the whole rectangle.
    ///
    /// This is what lets a caller holding the frost it computed for the same
    /// backdrop one frame ago — the compositor's retained backdrop, for a
    /// window that has since moved — keep every pixel no edge or shape
    /// difference can reach and recompute only the border that changed.
    ///
    /// The reads come from the surface, so the caller must have put back
    /// whatever it means the blur to read: the border's neighbourhood is
    /// `radius` pixels of the rectangle around it, which is the kept block
    /// eroded by `radius` at most. Keeping nothing frosts the whole rectangle.
    #[expect(
        clippy::too_many_arguments,
        reason = "frost_region's arguments plus the block to leave alone, \
                  spelled as the column and row ranges the passes take"
    )]
    pub fn frost_region_around(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        keep_cols: Range<u32>,
        keep_rows: Range<u32>,
        radius: u32,
        scratch: &mut BlurScratch,
        runner: &dyn JobRunner,
        coverage: impl Fn(u32, u32) -> u8 + Sync,
    ) {
        self.frost(
            x,
            y,
            w,
            h,
            Some((keep_cols, keep_rows)),
            radius,
            scratch,
            runner,
            coverage,
        );
    }

    /// Frost `[x, x+w) × [y, y+h)` except `keep`, or the whole of it for
    /// `None`.
    ///
    /// The one frost: both entry points are this call, so a border and a whole
    /// cannot round, replicate, weight, or dither differently.
    ///
    /// Neither pass reads a copy of the rectangle — the horizontal one reads
    /// the surface's own rows and the vertical one reads what it wrote — so a
    /// frost costs two passes over what it writes rather than three over the
    /// rectangle.
    ///
    /// # How it is split
    ///
    /// The work becomes **pieces**: the up-to-four row bands a kept block leaves,
    /// each divided across its columns so `runner`'s participants share it. A
    /// piece is independent of every other because both of its passes read only
    /// the *surface* and write only its own scratch — and a piece's answer is
    /// bit-for-bit what the undivided band would have written there, because
    /// replication and coverage are read from the whole rectangle whichever part
    /// of it a pass writes.
    ///
    /// Blurring and mixing stay two phases with a barrier between them, and split
    /// differently: blurring writes the scratch, so its pieces are the column
    /// divisions; mixing writes the *surface*, so its pieces are row bands of it.
    /// The barrier is not an artefact of splitting — a band's neighbourhood
    /// reaches into the bands beside it, and a frosted pixel is not the backdrop
    /// pixel the blur is a function of, so writing one band and then reading it as
    /// another's neighbour would seam.
    #[expect(
        clippy::too_many_arguments,
        reason = "the two public entry points' arguments, with the kept block \
                  they differ by; splitting it would duplicate the frost"
    )]
    fn frost(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        keep: Option<(Range<u32>, Range<u32>)>,
        radius: u32,
        scratch: &mut BlurScratch,
        runner: &dyn JobRunner,
        coverage: impl Fn(u32, u32) -> u8 + Sync,
    ) {
        if radius == 0 {
            return;
        }
        let Some((columns, rows)) = self.admitted(x, y, w, h) else {
            return;
        };
        let radius = usize::try_from(radius).unwrap_or(usize::MAX);
        let frost = Frost {
            x,
            y,
            columns,
            rows,
            radius,
            recip: Reciprocal::new(radius.saturating_mul(2).saturating_add(1)),
        };
        let BlurScratch {
            frosted,
            aux,
            pieces,
        } = scratch;
        frost.push_pieces(pieces, keep, runner);
        // Each piece is blurred at the same time as the others, so each needs its
        // own intermediate rather than taking turns in one.
        let (Some(output), Some(intermediate)) = (
            pieces
                .iter()
                .try_fold(0usize, |total, piece| total.checked_add(piece.area())),
            pieces.iter().try_fold(0usize, |total, piece| {
                total.checked_add(frost.intermediate(piece))
            }),
        ) else {
            return;
        };
        if output == 0 || !grow(frosted, output) || !grow(aux, intermediate) {
            return;
        }
        // One piece is the ordinary case — a serial runner, or a frost too small
        // to be worth dividing — and it is served from the stack, so a frost that
        // is not spread allocates nothing at all.
        if let [only] = pieces.as_slice() {
            let (Some(out), Some(mid)) = (
                frosted.get_mut(..only.area()),
                aux.get_mut(..frost.intermediate(only)),
            ) else {
                return;
            };
            let mut one = [BlurPiece {
                band: only.clone(),
                aux: mid,
                frosted: out,
            }];
            frost.blur_pieces(self, &mut one, runner);
            frost.mix_pieces(self, &one, runner, &coverage);
            return;
        }
        let mut split: Vec<BlurPiece<'_>> = Vec::new();
        if split.try_reserve_exact(pieces.len()).is_err() {
            return;
        }
        let mut out_rest: &mut [Pixel] = frosted;
        let mut mid_rest: &mut [Pixel] = aux;
        for band in pieces.iter() {
            let (area, span) = (band.area(), frost.intermediate(band));
            if area > out_rest.len() || span > mid_rest.len() {
                return;
            }
            let (out, rest) = out_rest.split_at_mut(area);
            out_rest = rest;
            let (mid, rest) = mid_rest.split_at_mut(span);
            mid_rest = rest;
            split.push(BlurPiece {
                band: band.clone(),
                aux: mid,
                frosted: out,
            });
        }
        frost.blur_pieces(self, &mut split, runner);
        frost.mix_pieces(self, &split, runner, &coverage);
    }
}

/// One independent piece of a frost: the part of the rectangle it answers for,
/// the intermediate its horizontal pass hands to its vertical one, and the
/// blurred pixels it produces.
struct BlurPiece<'a> {
    band: Band,
    aux: &'a mut [Pixel],
    frosted: &'a mut [Pixel],
}

/// The rectangle a frost is a function of: the surface columns and rows it
/// admits, the caller's own origin the shape coverage is read from, and the
/// blur it spreads by.
///
/// Replication and coverage are read from *this* rectangle whichever part of
/// it a call writes, which is what makes frosting a border equal frosting the
/// whole and keeping the middle.
struct Frost {
    x: u32,
    y: u32,
    columns: Range<u32>,
    rows: Range<u32>,
    radius: usize,
    recip: Reciprocal,
}

/// A part of a frosted rectangle one pass writes, in that rectangle's own
/// columns and rows.
#[derive(Clone)]
struct Band {
    cols: Range<usize>,
    rows: Range<usize>,
}

impl Band {
    /// The band covering no pixels, which a caller filters out: the placeholder
    /// for a border band a kept block flush against an edge leaves out.
    const fn none() -> Self {
        Self {
            cols: 0..0,
            rows: 0..0,
        }
    }

    fn is_empty(&self) -> bool {
        self.cols.is_empty() || self.rows.is_empty()
    }

    fn area(&self) -> usize {
        self.cols.len().saturating_mul(self.rows.len())
    }
}

impl Frost {
    fn width(&self) -> usize {
        usize::try_from(self.columns.end - self.columns.start).unwrap_or(0)
    }

    fn height(&self) -> usize {
        usize::try_from(self.rows.end - self.rows.start).unwrap_or(0)
    }

    /// The row bands a frost writes when `keep` is left alone: the whole
    /// rectangle when nothing is kept, otherwise the border around the kept block
    /// as the four bands that tile it — full width above and below it, and the two
    /// sides between.
    fn row_bands(&self, keep: Option<(Range<u32>, Range<u32>)>) -> [Band; 4] {
        let (width, height) = (self.width(), self.height());
        let whole = || {
            [
                Band {
                    cols: 0..width,
                    rows: 0..height,
                },
                Band::none(),
                Band::none(),
                Band::none(),
            ]
        };
        let Some((cols, rows)) = keep
            .map(|(cols, rows)| (within(cols, width), within(rows, height)))
            .filter(|(cols, rows)| !cols.is_empty() && !rows.is_empty())
        else {
            return whole();
        };
        [
            Band {
                cols: 0..width,
                rows: 0..rows.start,
            },
            Band {
                cols: 0..width,
                rows: rows.end..height,
            },
            Band {
                cols: 0..cols.start,
                rows: rows.clone(),
            },
            Band {
                cols: cols.end..width,
                rows,
            },
        ]
    }

    /// Fill `out` with the independent pieces this frost splits into: each row
    /// band `keep` leaves, divided across its columns into as many pieces as
    /// `runner` is worth splitting it for.
    ///
    /// Dividing by *columns* is what keeps a piece independent: both of a piece's
    /// passes read the surface and write only its own scratch, and the horizontal
    /// pass reads the whole rectangle's row whatever part of it it writes, so the
    /// replication at the rectangle's edges is the same for every piece.
    fn push_pieces(
        &self,
        out: &mut Vec<Band>,
        keep: Option<(Range<u32>, Range<u32>)>,
        runner: &dyn JobRunner,
    ) {
        out.clear();
        for band in self.row_bands(keep) {
            if band.is_empty() {
                continue;
            }
            // One piece per participant and no more. Every piece primes its
            // sliding window afresh at its own first column, which costs `radius`
            // samples per row that the undivided pass paid once — so dividing
            // more finely than there are participants to run the pieces buys
            // nothing and charges for it. The pieces are equal in size and shape,
            // so there is no imbalance for a finer division to absorb either.
            let share = band.area().div_ceil(runner.width().max(1));
            let count =
                tairix_parallel::bands(runner, band.area(), share.max(MIN_PARALLEL_FROST_PX));
            let per = band.cols.len().div_ceil(count.max(1)).max(1);
            let mut at = band.cols.start;
            while at < band.cols.end {
                let end = at.saturating_add(per).min(band.cols.end);
                if out.try_reserve(1).is_err() {
                    // Without room for every piece the split would be partial,
                    // and a partial split is a partial frost; keep what is there
                    // and stop dividing.
                    return;
                }
                out.push(Band {
                    cols: at..end,
                    rows: band.rows.clone(),
                });
                at = end;
            }
        }
    }

    /// Blur every piece into its own scratch. The surface is only read, so every
    /// piece reads the same backdrop the undivided band would have.
    fn blur_pieces(&self, surface: &Surface, pieces: &mut [BlurPiece<'_>], runner: &dyn JobRunner) {
        tairix_parallel::for_each(runner, pieces, &|piece| {
            let BlurPiece { band, aux, frosted } = piece;
            self.blur_band(surface, band, aux, frosted);
        });
    }

    /// Mix every piece's blurred pixels back over the surface.
    ///
    /// The blurred pixels are finished and read-only from here, so this phase is
    /// split by the rows it *writes* rather than by the pieces that produced them:
    /// a row band owns whole surface rows, and the pieces landing in them write
    /// disjoint columns of those rows.
    fn mix_pieces(
        &self,
        surface: &mut Surface,
        pieces: &[BlurPiece<'_>],
        runner: &dyn JobRunner,
        coverage: &(impl Fn(u32, u32) -> u8 + Sync),
    ) {
        let span = usize::try_from(self.rows.end.saturating_sub(self.rows.start)).unwrap_or(0);
        let count = tairix_parallel::bands(
            runner,
            span,
            MIN_PARALLEL_FROST_PX.div_ceil(self.width().max(1)),
        );
        let per = u32::try_from(span.div_ceil(count.max(1)))
            .unwrap_or(u32::MAX)
            .max(1);
        let mut bands = surface.row_bands_mut(self.rows.clone(), per);
        if count <= 1 {
            if let Some(mut only) = bands.next() {
                self.mix_rows(&mut only, pieces, coverage);
            }
            return;
        }
        let mut split: Vec<RowBand<'_>> = bands.collect();
        tairix_parallel::for_each(runner, &mut split, &|band| {
            self.mix_rows(band, pieces, coverage);
        });
    }

    /// The rows the horizontal pass must produce for `band`: `radius` beyond it
    /// on both sides, or the rectangle's own edge, whichever comes first —
    /// which is exactly where the replication begins.
    fn pass_rows(&self, band: &Band) -> Range<usize> {
        band.rows.start.saturating_sub(self.radius)
            ..band.rows.end.saturating_add(self.radius).min(self.height())
    }

    /// The intermediate pixels blurring `band` needs.
    fn intermediate(&self, band: &Band) -> usize {
        band.cols.len().saturating_mul(self.pass_rows(band).len())
    }

    /// Blur `band` of `surface` into `blurred`, carrying the horizontal pass
    /// over to the vertical one through `aux`. Reads `surface` and never
    /// writes it.
    fn blur_band(&self, surface: &Surface, band: &Band, aux: &mut [Pixel], blurred: &mut [Pixel]) {
        let (stride, span) = (band.cols.len(), self.columns.end - self.columns.start);
        let pass_rows = self.pass_rows(band);
        for (offset, target) in pass_rows.clone().zip(aux.chunks_exact_mut(stride)) {
            let Ok(offset) = u32::try_from(offset) else {
                continue;
            };
            let Some((_, source)) = surface.row_span(
                self.rows.start.saturating_add(offset),
                self.columns.start,
                span,
            ) else {
                continue;
            };
            blur_span(
                source,
                target,
                1,
                self.width(),
                self.radius,
                self.recip,
                band.cols.clone(),
            );
        }
        let vertical = band.rows.start - pass_rows.start..band.rows.end - pass_rows.start;
        for column in 0..stride {
            let (Some(source), Some(target)) = (aux.get(column..), blurred.get_mut(column..))
            else {
                continue;
            };
            blur_span(
                source,
                target,
                stride,
                pass_rows.len(),
                self.radius,
                self.recip,
                vertical.clone(),
            );
        }
    }

    /// Mix back, over the surface rows `band` owns, whatever every piece of
    /// `pieces` blurred there — each pixel weighted by its own `coverage` and
    /// rounded at the surface's ordered dither.
    ///
    /// A piece that reaches none of these rows contributes nothing, so this costs
    /// one range intersection per piece plus the pixels it actually writes.
    fn mix_rows(
        &self,
        band: &mut RowBand<'_>,
        pieces: &[BlurPiece<'_>],
        coverage: &impl Fn(u32, u32) -> u8,
    ) {
        let owned = band.rows();
        for piece in pieces {
            let (Ok(left), Ok(stride)) = (
                u32::try_from(piece.band.cols.start),
                u32::try_from(piece.band.cols.len()),
            ) else {
                continue;
            };
            let (Ok(from), Ok(until)) = (
                u32::try_from(piece.band.rows.start),
                u32::try_from(piece.band.rows.end),
            ) else {
                continue;
            };
            if stride == 0 {
                continue;
            }
            let lo = owned.start.max(self.rows.start.saturating_add(from));
            let hi = owned.end.min(self.rows.start.saturating_add(until));
            let lead = self.columns.start - self.x + left;
            for row in lo..hi {
                let Some(at) = row
                    .checked_sub(self.rows.start)
                    .and_then(|offset| offset.checked_sub(from))
                    .and_then(|local| usize::try_from(local).ok())
                    .and_then(|local| local.checked_mul(piece.band.cols.len()))
                else {
                    continue;
                };
                let Some(blurred) = piece
                    .frosted
                    .get(at..at.saturating_add(piece.band.cols.len()))
                else {
                    continue;
                };
                let Some((first, target)) =
                    band.row_span_mut(row, self.columns.start.saturating_add(left), stride)
                else {
                    continue;
                };
                let ly = row - self.y;
                let dither = DitherRow::at(row);
                for (((dst, src), lx), column) in
                    target.iter_mut().zip(blurred).zip(lead..).zip(first..)
                {
                    *dst = mix(*dst, *src, coverage(lx, ly), dither.bias(column));
                }
            }
        }
    }
}

/// `asked` confined to `0..extent`, empty where it lands wholly outside.
fn within(asked: Range<u32>, extent: usize) -> Range<usize> {
    let start = usize::try_from(asked.start)
        .unwrap_or(usize::MAX)
        .min(extent);
    let end = usize::try_from(asked.end).unwrap_or(usize::MAX).min(extent);
    start..end.max(start)
}

/// Row `y` of `src` and of `dst`, both `width` pixels wide.
fn row_pair<'a>(
    src: &'a [Pixel],
    dst: &'a mut [Pixel],
    y: usize,
    width: usize,
) -> Option<(&'a [Pixel], &'a mut [Pixel])> {
    let start = y.checked_mul(width)?;
    let end = start.checked_add(width)?;
    Some((src.get(start..end)?, dst.get_mut(start..end)?))
}

/// Column `x` of `src` and of `dst`, each the `count`-pixel block's tail
/// from that column on: the strided [`blur_span`] walks it a row at a time.
fn column_pair<'a>(
    src: &'a [Pixel],
    dst: &'a mut [Pixel],
    x: usize,
    count: usize,
) -> Option<(&'a [Pixel], &'a mut [Pixel])> {
    Some((src.get(x..count)?, dst.get_mut(x..count)?))
}

/// Average the samples of `src` with their neighbours within `radius`,
/// writing the outputs `out` names to `dst`. Both are walked with `stride`
/// pixels between consecutive samples, so one implementation serves the
/// horizontal pass (stride `1`) and the vertical one (stride `width`).
///
/// `src` is the whole line of `len` samples, indexed from its own start; `dst`
/// receives `out.len()` pixels from *its* start, so a caller taking part of a
/// line writes a buffer only that wide. `out` is confined to the line, and an
/// empty one writes nothing.
///
/// The window slides by adding the sample entering it and subtracting the
/// one leaving, so each output costs a constant amount of work whatever the
/// radius. Samples outside `0..len` replicate the nearest edge, which keeps
/// the divisor at `2 * radius + 1` for every output — constant for the whole
/// pass, which is why `recip` is resolved once by the caller. That
/// replication is read from the *line*, never from where `out` begins, which
/// is what makes a part of a line answer exactly as the whole of it does.
///
/// The output slot and the two samples the window trades are each monotone
/// along the line, so all three are walked as strided iterators and the
/// furthest offset any of them can reach is bounds-checked once here instead
/// of per sample. An iterator that runs out is exactly a clamped end, so the
/// replicated edge pixel stands in for it — which is why `src` is confined to
/// the line before the walk begins rather than trusted to end with it: a
/// caller passing a buffer with further pixels after the line (a band of a
/// larger scratch) would otherwise read one of those as a replicated edge.
fn blur_span(
    src: &[Pixel],
    dst: &mut [Pixel],
    stride: usize,
    len: usize,
    radius: usize,
    recip: Reciprocal,
    out: Range<usize>,
) {
    let Some(last) = len.checked_sub(1) else {
        return;
    };
    let Some(last_offset) = last.checked_mul(stride) else {
        return;
    };
    let Some(src) = src.get(..=last_offset) else {
        return;
    };
    let (Some(&first), Some(&edge)) = (src.first(), src.get(last_offset)) else {
        return;
    };
    let from = out.start.min(len);
    let Some(count) = out
        .end
        .min(len)
        .checked_sub(from)
        .filter(|asked| *asked > 0)
    else {
        return;
    };
    if dst.len() <= (count - 1).saturating_mul(stride) {
        return;
    }

    // Prime the window over `from - radius ..= from + radius`. The replicated
    // ends are counted arithmetically rather than sample by sample, so priming
    // costs the line's length at most however wide the radius is.
    let (lead, trail) = (
        from.saturating_sub(radius),
        from.saturating_add(radius).min(last),
    );
    let mut sum = Sum::default();
    sum.add_many(first, radius.saturating_sub(from));
    for &pixel in src
        .get(lead.saturating_mul(stride)..)
        .unwrap_or_default()
        .iter()
        .step_by(stride)
        .take(trail - lead + 1)
    {
        sum.add(pixel);
    }
    sum.add_many(edge, from.saturating_add(radius).saturating_sub(last));

    let mut entering = src
        .get(trail.saturating_add(1).min(last).saturating_mul(stride)..)
        .unwrap_or_default()
        .iter()
        .step_by(stride);
    // The outputs whose trailing edge has not yet cleared the start of the
    // line, whose leaving sample is therefore the replicated first pixel.
    // Splitting the walk there costs that clamp nothing at all.
    let clamped = radius.saturating_add(1).saturating_sub(from).min(count);
    let mut leaving = src
        .get(
            from.saturating_add(clamped)
                .saturating_sub(radius)
                .saturating_mul(stride)..,
        )
        .unwrap_or_default()
        .iter()
        .step_by(stride);
    let mut slots = dst.iter_mut().step_by(stride).take(count);

    for slot in slots.by_ref().take(clamped) {
        *slot = sum.mean(recip);
        sum.add(*entering.next().unwrap_or(&edge));
        sum.sub(first);
    }
    for slot in slots {
        *slot = sum.mean(recip);
        sum.add(*entering.next().unwrap_or(&edge));
        sum.sub(*leaving.next().unwrap_or(&first));
    }
}

/// The running channel sums of the samples currently inside the sliding
/// window.
///
/// A `u32` per channel is ample: a channel is at most 255 and the window
/// holds at most one screen dimension's worth of samples, so the sum cannot
/// approach the type's range for any radius a surface is drawn at. Every
/// operation saturates so that a caller passing an absurd radius — the
/// entry point is public and takes any `usize` — gets a flattened region
/// rather than an arithmetic panic.
#[derive(Copy, Clone, Default)]
struct Sum {
    r: u32,
    g: u32,
    b: u32,
    a: u32,
}

impl Sum {
    /// Add `times` copies of `pixel` to the window.
    fn add_many(&mut self, pixel: Pixel, times: usize) {
        let times = u32::try_from(times).unwrap_or(u32::MAX);
        let weighted = |channel: u8| u32::from(channel).saturating_mul(times);
        self.r = self.r.saturating_add(weighted(pixel.r));
        self.g = self.g.saturating_add(weighted(pixel.g));
        self.b = self.b.saturating_add(weighted(pixel.b));
        self.a = self.a.saturating_add(weighted(pixel.a));
    }

    /// Add one copy of `pixel` to the window.
    fn add(&mut self, pixel: Pixel) {
        self.r = self.r.saturating_add(u32::from(pixel.r));
        self.g = self.g.saturating_add(u32::from(pixel.g));
        self.b = self.b.saturating_add(u32::from(pixel.b));
        self.a = self.a.saturating_add(u32::from(pixel.a));
    }

    /// Remove one copy of `pixel` from the window.
    fn sub(&mut self, pixel: Pixel) {
        self.r = self.r.saturating_sub(u32::from(pixel.r));
        self.g = self.g.saturating_sub(u32::from(pixel.g));
        self.b = self.b.saturating_sub(u32::from(pixel.b));
        self.a = self.a.saturating_sub(u32::from(pixel.a));
    }

    /// The window's mean over `recip`'s divisor, rounded to nearest.
    fn mean(self, recip: Reciprocal) -> Pixel {
        Pixel {
            r: recip.apply(self.r),
            g: recip.apply(self.g),
            b: recip.apply(self.b),
            a: recip.apply(self.a),
        }
    }
}

/// The fractional bits the reciprocal multiply is computed with.
const RECIPROCAL_SHIFT: u32 = 40;

/// The largest window the reciprocal multiply is exactly equal to the divide
/// for, and therefore the largest it is used at.
const RECIPROCAL_MAX_COUNT: u32 = 65_536;

/// How one window's mean is divided by its sample count.
///
/// The count is `2 * radius + 1` for every output of a pass, so it is resolved
/// once for the pass instead of dividing four times per pixel per pass — which
/// was the dominant cost of a frosted window.
#[derive(Copy, Clone)]
enum Reciprocal {
    /// Multiply by a fixed-point reciprocal, which for every window the blur
    /// is used at gives *exactly* the same answer as the divide.
    ///
    /// The target is `floor((n + d/2) / d)`, so with `n` the rounded numerator
    /// and `m = ceil(2^S / d)` the claim is `floor(n*m / 2^S) == floor(n/d)`.
    /// Write `n = k*d + s` with `s <= d-1`, and `e = m*d - 2^S` (so `e <= d-1`
    /// by construction). Then `n*m/2^S = n/d + n*e/(d*2^S)`, and the two floors
    /// agree exactly while `s*2^S + n*e < d*2^S`; since `s <= d-1` it suffices
    /// that `n*e < 2^S`.
    ///
    /// A window holds exactly `d` samples of at most 255 each, so `n < 256*d`,
    /// and `(256*d - 1) * (d - 1) < 2^40` holds for every `d` up to 65536 —
    /// which is why that is the cutoff, and why the product stays under `2^48`.
    /// `blur_tests` checks the condition for every count in range, and that a
    /// count above it genuinely breaks the proof, rather than leaving either
    /// argued.
    ///
    /// The product saturates so the answer stays a total function of its
    /// argument: a numerator from outside a window of `d` samples — which no
    /// blur produces — reads as fully bright rather than overflowing.
    Multiply { m: u64, half: u32 },
    /// Divide, for a count past the range the multiply is exact over. No
    /// surface is drawn at such a radius; correctness does not depend on that.
    Divide { d: u32 },
}

impl Reciprocal {
    /// The divisor for a window of `count` samples. A count of zero would make
    /// no window, so it reads as one.
    fn new(count: usize) -> Self {
        let d = u32::try_from(count).unwrap_or(u32::MAX).max(1);
        if d <= RECIPROCAL_MAX_COUNT {
            let m = (1u64 << RECIPROCAL_SHIFT).div_ceil(u64::from(d));
            Self::Multiply { m, half: d / 2 }
        } else {
            Self::Divide { d }
        }
    }

    /// One channel's rounded mean, clamped to the channel range.
    #[inline]
    fn apply(self, sum: u32) -> u8 {
        let rounded = match self {
            Self::Multiply { m, half } => {
                u64::from(sum.saturating_add(half)).saturating_mul(m) >> RECIPROCAL_SHIFT
            }
            Self::Divide { d } => u64::from(sum.saturating_add(d / 2)) / u64::from(d),
        };
        u8::try_from(rounded.min(255)).unwrap_or(u8::MAX)
    }
}

#[cfg(test)]
#[path = "blur_tests.rs"]
mod tests;
