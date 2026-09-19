use super::{
    chunk_origin, lerp, quantise_i16, quantise_u16, quantise_u8, signed, smoothstep, CellCoord,
    Elevation, Moisture, Temperature, CELL_SUB_UNITS, CHUNK_AREA, CHUNK_CELLS, ELEVATION_SUB_UNITS,
};

#[test]
fn chunk_geometry_is_consistent() {
    assert!(CHUNK_CELLS.is_power_of_two());
    assert_eq!(CHUNK_AREA, (CHUNK_CELLS as usize).pow(2));
    assert!(u32::try_from(CELL_SUB_UNITS)
        .expect("positive")
        .is_power_of_two());
    assert!(u32::try_from(ELEVATION_SUB_UNITS)
        .expect("positive")
        .is_power_of_two());
}

#[test]
fn cells_split_into_chunk_and_offset_across_the_origin() {
    for x in -200_i32..200 {
        for y in [-129_i32, -64, -1, 0, 1, 63, 64, 130] {
            let cell = CellCoord::new(x, y);
            let chunk = cell.chunk();
            let (ox, oy) = cell.within_chunk();
            assert!(ox < CHUNK_CELLS && oy < CHUNK_CELLS);
            let origin = chunk_origin(chunk);
            assert_eq!(
                (origin.x + signed(ox), origin.y + signed(oy)),
                (cell.x, cell.y),
                "chunk and offset must reconstruct the cell"
            );
        }
    }
}

#[test]
fn the_chunk_grid_has_no_seam_at_the_origin() {
    // Division toward zero would put -1 and 0 in the same chunk.
    assert_eq!(CellCoord::new(-1, -1).chunk().x, -1);
    assert_eq!(CellCoord::new(0, 0).chunk().x, 0);
}

#[test]
fn cell_centres_stay_inside_the_representable_world() {
    assert!(CellCoord::new(0, 0).centre().is_some());
    assert!(CellCoord::new(i32::MAX, 0).centre().is_none());
}

#[test]
fn elevation_round_trips_exactly_on_its_power_of_two_scale() {
    for eighths in -400_i32..400 {
        let units = f64::from(eighths) / f64::from(ELEVATION_SUB_UNITS);
        assert!((Elevation::from_units(units).units() - units).abs() < f64::EPSILON);
    }
}

#[test]
fn quantisation_saturates_rather_than_wrapping() {
    assert_eq!(quantise_i16(1.0e9), i16::MAX);
    assert_eq!(quantise_i16(-1.0e9), i16::MIN);
    assert_eq!(quantise_u16(-5.0), 0);
    assert_eq!(quantise_u16(1.0e9), u16::MAX);
    assert_eq!(quantise_u8(-5.0), 0);
    assert_eq!(quantise_u8(1.0e9), u8::MAX);
}

#[test]
fn elevation_saturates_rather_than_wrapping() {
    assert_eq!(Elevation::from_units(1.0e9), Elevation::MAX);
    assert_eq!(Elevation::from_units(-1.0e9), Elevation::MIN);
}

#[test]
fn sea_level_is_submerged_and_anything_above_it_is_not() {
    assert!(Elevation::SEA_LEVEL.is_submerged());
    assert!(Elevation(-1).is_submerged());
    assert!(!Elevation(1).is_submerged());
}

#[test]
fn moisture_clamps_to_its_fraction() {
    assert_eq!(Moisture::from_fraction(-1.0), Moisture(0));
    assert_eq!(Moisture::from_fraction(2.0), Moisture(u16::MAX));
    assert!((Moisture::from_fraction(0.5).fraction() - 0.5).abs() < 1.0e-4);
}

#[test]
fn temperature_round_trips_within_its_step() {
    for whole in -90_i32..=60 {
        let celsius = f64::from(whole);
        assert!((Temperature::from_celsius(celsius).celsius() - celsius).abs() < f64::EPSILON);
    }
}

#[test]
fn lerp_reproduces_both_endpoints() {
    assert!((lerp(3.0, 9.0, 0.0) - 3.0).abs() < f64::EPSILON);
    assert!((lerp(3.0, 9.0, 1.0) - 9.0).abs() < f64::EPSILON);
}

#[test]
fn smoothstep_is_clamped_and_monotone() {
    assert!(smoothstep(-1.0).abs() < f64::EPSILON);
    assert!((smoothstep(2.0) - 1.0).abs() < f64::EPSILON);
    let mut previous = 0.0;
    for step in 0..=100 {
        let value = smoothstep(f64::from(step) / 100.0);
        assert!(value >= previous - f64::EPSILON);
        previous = value;
    }
}
