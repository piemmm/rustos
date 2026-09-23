//! The role table's own consistency.

use tairix_raster::Color;

use super::{Tint, Tints};

#[test]
fn every_role_has_its_own_slot() {
    for (position, tint) in Tint::ALL.into_iter().enumerate() {
        assert_eq!(tint.index(), position, "{tint:?} is out of table order");
    }
}

#[test]
fn a_table_answers_each_role_with_its_own_colour() {
    let mut colours = [Color::rgb(0, 0, 0); Tint::COUNT];
    for (slot, colour) in colours.iter_mut().enumerate() {
        let shade = u8::try_from(slot * 30).expect("a small slot");
        *colour = Color::rgb(shade, shade, shade);
    }
    let tints = Tints::new(colours);
    for tint in Tint::ALL {
        assert_eq!(tints.get(tint), colours[tint.index()]);
    }
}
