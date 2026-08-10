//! [`Region`]: a set of screen pixels held as disjoint rectangles.
//!
//! Damage tracking, control repaint reporting and clipping all need the same
//! thing — "which pixels does this cover?" — so the set lives here once, beside
//! the [`Rect`] it is built from, rather than once per consumer.
//!
//! The rectangles are kept in a canonical *band* form: rows of the screen are
//! cut into horizontal bands, each band holds its covered x-spans in
//! left-to-right order, and no two spans in a band touch. Two regions covering
//! the same pixels therefore have the same rectangles, which is what makes
//! equality meaningful and lets [`add`](Region::add) run as a single merge walk
//! instead of rescanning every rectangle against every other.

use alloc::vec::Vec;

use crate::{span, Point, Rect};

/// A set of screen pixels, held as pairwise-disjoint rectangles.
///
/// Every mutation leaves the rectangles canonical: band-ordered (top to
/// bottom, then left to right), never overlapping, and never touching within a
/// band. [`rects`](Region::rects) is therefore both the iteration order and a
/// promise that no pixel is listed twice — a consumer may composite, copy or
/// present each rectangle in turn and pay for each covered pixel exactly once.
///
/// A region built with [`with_budget`](Region::with_budget) trades precision
/// for a bounded rectangle count: once a mutation would leave more rectangles
/// than the budget allows the region degrades to its bounding box. Covering
/// more than was asked for is always safe — a repaint is idempotent — so the
/// budget is a work bound the consumer chooses from its own per-rectangle cost,
/// not a correctness limit. [`new`](Region::new) imposes none and stays exact.
#[derive(Default)]
pub struct Region {
    rects: Vec<Rect>,
    /// Kept for reuse across mutations so a frame's worth of edits allocates
    /// once rather than once per edit.
    scratch: Vec<Rect>,
    bounds: Rect,
    budget: Option<usize>,
}

/// How two canonical rectangle lists are combined.
#[derive(Copy, Clone)]
enum Op {
    Union,
    Intersect,
    Subtract,
}

impl Region {
    /// An empty region that keeps every rectangle it is given.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rects: Vec::new(),
            scratch: Vec::new(),
            bounds: Rect::EMPTY,
            budget: None,
        }
    }

    /// An empty region that degrades to its bounding box rather than hold more
    /// than `max_rects` rectangles.
    ///
    /// A budget of zero is raised to one: a non-empty region always keeps at
    /// least its bounding box.
    #[must_use]
    pub const fn with_budget(max_rects: usize) -> Self {
        Self {
            rects: Vec::new(),
            scratch: Vec::new(),
            bounds: Rect::EMPTY,
            budget: Some(if max_rects == 0 { 1 } else { max_rects }),
        }
    }

    /// The rectangle budget, or `None` when the region is exact.
    #[must_use]
    pub const fn budget(&self) -> Option<usize> {
        self.budget
    }

    /// `true` when the region covers no pixels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The disjoint rectangles covering the region, band-ordered.
    #[must_use]
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// The bounding box of the whole region, or [`Rect::EMPTY`].
    #[must_use]
    pub const fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Forget every pixel.
    pub fn clear(&mut self) {
        self.rects.clear();
        self.bounds = Rect::EMPTY;
    }

    /// Add every pixel of `rect`. Empty rectangles cover nothing and are
    /// ignored.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        if self.rects.is_empty() {
            self.rects.push(rect);
            self.bounds = rect;
            return;
        }
        self.apply(rect, Op::Union);
    }

    /// Remove every pixel of `rect`.
    pub fn subtract(&mut self, rect: Rect) {
        if rect.is_empty() || self.rects.is_empty() {
            return;
        }
        if rect.intersection(&self.bounds).is_empty() {
            return;
        }
        self.apply(rect, Op::Subtract);
    }

    /// Keep only the pixels inside `rect`.
    pub fn clip(&mut self, rect: Rect) {
        if self.rects.is_empty() {
            return;
        }
        if rect.is_empty() {
            self.clear();
            return;
        }
        self.apply(rect, Op::Intersect);
    }

    /// Shift every pixel by `(dx, dy)`.
    ///
    /// A shift that would carry a coordinate outside the `i32` range collapses
    /// the region to its clamped bounding box: covering more than was asked for
    /// keeps a repaint correct, where wrapping or silently dropping a
    /// rectangle would not.
    pub fn translate(&mut self, dx: i32, dy: i32) {
        if self.rects.is_empty() || (dx == 0 && dy == 0) {
            return;
        }
        if shift_fits(self.bounds, dx, dy) {
            for rect in &mut self.rects {
                rect.origin.x += dx;
                rect.origin.y += dy;
            }
            self.bounds.origin.x += dx;
            self.bounds.origin.y += dy;
            return;
        }
        let clamped = Rect::new(
            clamp_i32(i64::from(self.bounds.left()) + i64::from(dx)),
            clamp_i32(i64::from(self.bounds.top()) + i64::from(dy)),
            self.bounds.width,
            self.bounds.height,
        );
        self.rects.clear();
        self.rects.push(clamped);
        self.bounds = clamped;
    }

    /// `true` when `point` lies inside the region.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.bounds.contains(point) && self.rects.iter().any(|r| r.contains(point))
    }

    /// `true` when the region shares at least one pixel with `rect`.
    #[must_use]
    pub fn intersects(&self, rect: Rect) -> bool {
        !rect.intersection(&self.bounds).is_empty()
            && self.rects.iter().any(|r| !r.intersection(&rect).is_empty())
    }

    /// Combine `rect` into the canonical rectangles under `op`, reusing the
    /// buffer the previous mutation left behind.
    fn apply(&mut self, rect: Rect, op: Op) {
        let mut out = core::mem::take(&mut self.scratch);
        combine(&self.rects, &[rect], op, &mut out);
        self.scratch = core::mem::replace(&mut self.rects, out);
        self.bounds = self
            .rects
            .iter()
            .fold(Rect::EMPTY, |acc, rect| acc.union(rect));
        if self.budget.is_some_and(|max| self.rects.len() > max) {
            self.rects.clear();
            self.rects.push(self.bounds);
        }
    }
}

impl Clone for Region {
    fn clone(&self) -> Self {
        Self {
            rects: self.rects.clone(),
            scratch: Vec::new(),
            bounds: self.bounds,
            budget: self.budget,
        }
    }
}

/// Two regions are equal when they cover the same pixels; the canonical form
/// makes that a plain comparison of the rectangles, and the budget is a
/// consumer's work policy rather than part of the set.
impl PartialEq for Region {
    fn eq(&self, other: &Self) -> bool {
        self.rects == other.rects
    }
}

impl Eq for Region {}

/// The scratch buffer and the bounding box are derived state, so a dump shows
/// only what defines the set.
impl core::fmt::Debug for Region {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Region")
            .field("rects", &self.rects)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl From<Rect> for Region {
    fn from(rect: Rect) -> Self {
        let mut region = Self::new();
        region.add(rect);
        region
    }
}

/// `true` when shifting `bounds` by `(dx, dy)` keeps every edge inside `i32`.
fn shift_fits(bounds: Rect, dx: i32, dy: i32) -> bool {
    let fits = |edge: i32, delta: i32| i32::try_from(i64::from(edge) + i64::from(delta)).is_ok();
    fits(bounds.left(), dx)
        && fits(bounds.right(), dx)
        && fits(bounds.top(), dy)
        && fits(bounds.bottom(), dy)
}

/// `v` clamped into the `i32` range.
fn clamp_i32(v: i64) -> i32 {
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(i32::MAX)
}

/// The index one past the band that starts at `start`: the run of rectangles
/// sharing its top edge.
fn band_end(rects: &[Rect], start: usize) -> usize {
    let Some(first) = rects.get(start) else {
        return start;
    };
    let top = first.top();
    let mut end = start + 1;
    while rects.get(end).is_some_and(|r| r.top() == top) {
        end += 1;
    }
    end
}

/// Combine two canonical rectangle lists into `out`, itself canonical.
///
/// The two inputs are walked together in horizontal stripes, each stripe being
/// a range of rows over which neither side's spans change. Every stripe's spans
/// are combined once, so the walk is linear in the two inputs rather than
/// quadratic in either.
fn combine(a: &[Rect], b: &[Rect], op: Op, out: &mut Vec<Rect>) {
    out.clear();
    let mut ai = 0usize;
    let mut bi = 0usize;
    let mut y = match (a.first(), b.first()) {
        (Some(first_a), Some(first_b)) => first_a.top().min(first_b.top()),
        (Some(first), None) | (None, Some(first)) => first.top(),
        (None, None) => return,
    };
    // The band whose rows the previous emitted stripe covers, so a stripe that
    // repeats it extends it rather than adding a second band.
    let mut prev = None::<(usize, usize, i32)>;
    loop {
        while a.get(ai).is_some_and(|rect| rect.bottom() <= y) {
            ai = band_end(a, ai);
        }
        while b.get(bi).is_some_and(|rect| rect.bottom() <= y) {
            bi = band_end(b, bi);
        }
        let (a_head, b_head) = (a.get(ai).copied(), b.get(bi).copied());
        let done = match op {
            Op::Union => a_head.is_none() && b_head.is_none(),
            Op::Intersect => a_head.is_none() || b_head.is_none(),
            Op::Subtract => a_head.is_none(),
        };
        if done {
            return;
        }
        let mut end = i32::MAX;
        let mut a_band: &[Rect] = &[];
        let mut b_band: &[Rect] = &[];
        if let Some(head) = a_head {
            if head.top() <= y {
                a_band = a.get(ai..band_end(a, ai)).unwrap_or(&[]);
                end = end.min(head.bottom());
            } else {
                end = end.min(head.top());
            }
        }
        if let Some(head) = b_head {
            if head.top() <= y {
                b_band = b.get(bi..band_end(b, bi)).unwrap_or(&[]);
                end = end.min(head.bottom());
            } else {
                end = end.min(head.top());
            }
        }
        if end <= y {
            return;
        }
        emit_stripe(out, &mut prev, y, end, a_band, b_band, op);
        y = end;
    }
}

/// Emit the rows `[top, bottom)` of the combination of two bands' spans,
/// merging into the previous band when it holds the same spans on the rows
/// immediately above.
fn emit_stripe(
    out: &mut Vec<Rect>,
    prev: &mut Option<(usize, usize, i32)>,
    top: i32,
    bottom: i32,
    a: &[Rect],
    b: &[Rect],
    op: Op,
) {
    let start = out.len();
    match op {
        Op::Union => spans_union(out, top, bottom, a, b),
        Op::Intersect => spans_intersect(out, top, bottom, a, b),
        Op::Subtract => spans_subtract(out, top, bottom, a, b),
    }
    let len = out.len() - start;
    if len == 0 {
        return;
    }
    if let Some((prev_start, prev_len, prev_bottom)) = *prev {
        if prev_len == len && prev_bottom == top && same_spans(out, prev_start, start, len) {
            let grow = span(top, bottom);
            if let Some(band) = out.get_mut(prev_start..prev_start + prev_len) {
                for rect in band {
                    rect.height = rect.height.saturating_add(grow);
                }
            }
            out.truncate(start);
            *prev = Some((prev_start, prev_len, bottom));
            return;
        }
    }
    *prev = Some((start, len, bottom));
}

/// `true` when the `len` rectangles at `left` and at `right` cover the same
/// x-spans.
fn same_spans(rects: &[Rect], left: usize, right: usize, len: usize) -> bool {
    (0..len).all(|offset| {
        matches!(
            (rects.get(left + offset), rects.get(right + offset)),
            (Some(l), Some(r)) if l.origin.x == r.origin.x && l.width == r.width
        )
    })
}

/// Push the rows `[top, bottom)` of one x-span.
fn push_span(out: &mut Vec<Rect>, top: i32, bottom: i32, left: i32, right: i32) {
    if right > left {
        out.push(Rect::new(left, top, span(left, right), span(top, bottom)));
    }
}

/// The union of two bands' spans, coalescing spans that overlap or touch so the
/// result keeps the canonical "no two spans adjoin" form.
fn spans_union(out: &mut Vec<Rect>, top: i32, bottom: i32, left: &[Rect], right: &[Rect]) {
    let (mut li, mut ri) = (0usize, 0usize);
    let mut open: Option<(i32, i32)> = None;
    loop {
        let next = match (left.get(li), right.get(ri)) {
            (Some(lr), Some(rr)) => {
                if lr.left() <= rr.left() {
                    li += 1;
                    *lr
                } else {
                    ri += 1;
                    *rr
                }
            }
            (Some(r), None) => {
                li += 1;
                *r
            }
            (None, Some(r)) => {
                ri += 1;
                *r
            }
            (None, None) => break,
        };
        match open {
            Some((from, to)) if next.left() <= to => {
                open = Some((from, to.max(next.right())));
            }
            Some((from, to)) => {
                push_span(out, top, bottom, from, to);
                open = Some((next.left(), next.right()));
            }
            None => open = Some((next.left(), next.right())),
        }
    }
    if let Some((from, to)) = open {
        push_span(out, top, bottom, from, to);
    }
}

/// The intersection of two bands' spans.
fn spans_intersect(out: &mut Vec<Rect>, top: i32, bottom: i32, left: &[Rect], right: &[Rect]) {
    let (mut li, mut ri) = (0usize, 0usize);
    while let (Some(lr), Some(rr)) = (left.get(li), right.get(ri)) {
        push_span(
            out,
            top,
            bottom,
            lr.left().max(rr.left()),
            lr.right().min(rr.right()),
        );
        if lr.right() < rr.right() {
            li += 1;
        } else {
            ri += 1;
        }
    }
}

/// The spans of `keeps` with every span of `cuts` removed.
fn spans_subtract(out: &mut Vec<Rect>, top: i32, bottom: i32, keeps: &[Rect], cuts: &[Rect]) {
    let mut first = 0usize;
    for keep in keeps {
        // A cut wholly left of this span cannot touch any later one either, so
        // the cursor only ever moves forward.
        while cuts
            .get(first)
            .is_some_and(|cut| cut.right() <= keep.left())
        {
            first += 1;
        }
        let mut from = keep.left();
        let mut index = first;
        while let Some(cut) = cuts.get(index).filter(|cut| cut.left() < keep.right()) {
            push_span(out, top, bottom, from, cut.left().min(keep.right()));
            from = from.max(cut.right());
            if from >= keep.right() {
                break;
            }
            index += 1;
        }
        push_span(out, top, bottom, from, keep.right());
    }
}
