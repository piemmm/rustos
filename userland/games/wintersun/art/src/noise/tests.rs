use super::{centred, to_byte, Field, Tiled, MAX_PERIOD_LOG2};

const KEY: u64 = 0x5748_4954_4553_554E;

#[test]
fn a_period_outside_the_bound_is_refused() {
    assert!(Tiled::new(KEY, 0).is_none());
    assert!(Tiled::new(KEY, MAX_PERIOD_LOG2 + 1).is_none());
    assert!(Tiled::new(KEY, 1).is_some());
    assert!(Tiled::new(KEY, MAX_PERIOD_LOG2).is_some());
}

#[test]
fn a_tiled_field_wraps_at_its_period() {
    let period = 6;
    let side = 1i32 << period;
    let field = Tiled::new(KEY, period).expect("period is in range");
    for y in -3..3 {
        for x in -3..3 {
            assert_eq!(
                field.corner(Field::Grain, x, y),
                field.corner(Field::Grain, x + side, y),
            );
            assert_eq!(
                field.corner(Field::Grain, x, y),
                field.corner(Field::Grain, x, y + side),
            );
        }
    }
}

#[test]
fn an_unbounded_field_does_not_wrap() {
    let field = Tiled::unbounded(KEY);
    assert_ne!(
        field.corner(Field::WarpX, 0, 0),
        field.corner(Field::WarpX, 1 << 10, 0),
    );
}

#[test]
fn fields_are_domain_separated() {
    let field = Tiled::unbounded(KEY);
    let at = |f| field.corner(f, 7, -11);
    let values = [
        at(Field::Grain),
        at(Field::Relief),
        at(Field::WarpX),
        at(Field::WarpY),
        at(Field::Fray),
        at(Field::Spawn),
    ];
    for (i, a) in values.iter().enumerate() {
        for b in &values[i + 1..] {
            assert_ne!(a, b, "two fields agree at one lattice point");
        }
    }
}

#[test]
fn a_different_key_gives_a_different_field() {
    let a = Tiled::unbounded(KEY);
    let b = Tiled::unbounded(KEY ^ 1);
    assert_ne!(a.corner(Field::Grain, 3, 4), b.corner(Field::Grain, 3, 4));
}

#[test]
fn value_reproduces_the_corner_at_a_lattice_point() {
    let field = Tiled::unbounded(KEY);
    for cell_log2 in 1..8 {
        for (x, y) in [(0, 0), (1, 2), (-3, 5)] {
            let (px, py) = (x << cell_log2, y << cell_log2);
            assert_eq!(
                field.value(Field::Grain, px, py, cell_log2),
                field.corner(Field::Grain, x, y),
                "cell_log2={cell_log2} at ({x}, {y})",
            );
        }
    }
}

#[test]
fn value_is_continuous_across_the_origin() {
    // A field that discontinues at zero would put a visible line through
    // the middle of the world, which is the failure a `%` remainder makes
    // and an arithmetic shift does not.
    let field = Tiled::unbounded(KEY);
    let cell_log2 = 5;
    let mut previous = field.value(Field::Grain, -8, 0, cell_log2);
    for x in -7..=8 {
        let next = field.value(Field::Grain, x, 0, cell_log2);
        let step = i32::from(next).abs_diff(i32::from(previous));
        assert!(step < 6000, "jump of {step} at x={x}");
        previous = next;
    }
}

#[test]
fn value_wraps_seamlessly_at_the_tile_edge() {
    // The whole point of a tiled sampler: the interpolated value one unit
    // inside the left edge equals the value one unit inside the right.
    let period = 5;
    let cell_log2 = 3;
    let side = (1i32 << period) << cell_log2;
    let field = Tiled::new(KEY, period).expect("period is in range");
    for y in [0, 7, 19] {
        for x in 0..4 {
            assert_eq!(
                field.value(Field::Grain, x, y, cell_log2),
                field.value(Field::Grain, x + side, y, cell_log2),
            );
        }
    }
}

#[test]
fn fbm_with_no_octaves_is_the_mid_tone() {
    let field = Tiled::unbounded(KEY);
    assert_eq!(field.fbm(Field::Grain, 5, 9, 6, 0), 0x8000);
}

#[test]
fn fbm_stops_before_the_lattice_collapses() {
    // Asking for more octaves than the cell size has room for must not
    // divide by nothing, and must converge rather than change.
    let field = Tiled::unbounded(KEY);
    let deep = field.fbm(Field::Relief, 11, 13, 4, 40);
    // A cell of 2^4 admits four halvings before it reaches one unit.
    let enough = field.fbm(Field::Relief, 11, 13, 4, 4);
    assert_eq!(deep, enough);
    assert_ne!(deep, field.fbm(Field::Relief, 11, 13, 4, 3));
}

#[test]
fn fbm_adding_an_octave_changes_the_detail_but_not_the_shape() {
    let field = Tiled::unbounded(KEY);
    let coarse = field.fbm(Field::Grain, 100, 100, 7, 1);
    let fine = field.fbm(Field::Grain, 100, 100, 7, 4);
    assert_ne!(coarse, fine);
    // Each octave is half the amplitude of the last, so four octaves can
    // move the answer by at most the tail of the series.
    assert!(i32::from(coarse).abs_diff(i32::from(fine)) < 30_000);
}

#[test]
fn fbm_spans_a_useful_part_of_the_range() {
    // A field that only ever reports values near the middle produces a
    // flat material, which is the quiet failure of a badly weighted fbm.
    let field = Tiled::unbounded(KEY);
    let (mut low, mut high) = (u16::MAX, 0u16);
    for y in 0..64 {
        for x in 0..64 {
            let v = field.fbm(Field::Grain, x * 3, y * 3, 5, 3);
            low = low.min(v);
            high = high.max(v);
        }
    }
    assert!(low < 0x4000, "fbm never went dark: low={low}");
    assert!(high > 0xC000, "fbm never went light: high={high}");
}

#[test]
fn to_byte_maps_the_ends_exactly() {
    assert_eq!(to_byte(0), 0);
    assert_eq!(to_byte(u16::MAX), 255);
    assert_eq!(to_byte(0x8000), 128);
}

#[test]
fn centred_maps_the_ends_and_the_middle() {
    assert_eq!(centred(0, 100), -100);
    assert_eq!(centred(u16::MAX, 100), 100);
    assert_eq!(centred(0x8000, 100), 0);
    assert_eq!(centred(0x8000, 0), 0);
}

#[test]
fn centred_stays_inside_its_half_width() {
    for value in (0..=u16::MAX).step_by(97) {
        let offset = centred(value, 1 << 20);
        assert!((-(1 << 20)..=1 << 20).contains(&offset));
    }
}

#[test]
fn a_coarse_cell_does_not_overflow_the_fraction() {
    // Scaling the within-cell offset by the full u16 range overflowed a
    // 32-bit product for any cell above 2^16 — one step past the warp's
    // own 2^14.
    let field = Tiled::unbounded(KEY);
    for cell_log2 in [16, 17, 24, 30] {
        let value = field.value(Field::WarpX, i32::MAX - 7, i32::MIN + 13, cell_log2);
        let _ = value;
    }
}

#[test]
fn a_cell_beyond_the_coordinate_space_saturates_rather_than_shifting_off() {
    let field = Tiled::unbounded(KEY);
    let at_limit = field.value(Field::Grain, 1234, -5678, super::MAX_CELL_LOG2);
    for cell_log2 in [super::MAX_CELL_LOG2 + 1, 40, 64, u32::MAX] {
        assert_eq!(field.value(Field::Grain, 1234, -5678, cell_log2), at_limit);
    }
}
