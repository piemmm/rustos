//! Fixtures the crate's own tests share.

use crate::humanoid;
use crate::identity::{Build, Identity, Setting};
use crate::reference;
use crate::rig::Rig;
use crate::species::Species;

/// The reference human's rig: the figure a test poses when the species is
/// not its subject.
#[track_caller]
pub(crate) fn human() -> Rig {
    let identity =
        reference::identity(Species::Human).expect("the reference human is a real record");
    humanoid::rig(&identity).expect("the reference human builds")
}

/// Every one of `species`' 32 build corners, its reference figure's features
/// kept: each setting at its low or its high end, independently.
pub(crate) fn corners(species: Species) -> impl Iterator<Item = Identity> {
    (0u8..32).map(move |mask| {
        let end = |bit: u8| {
            if mask & (1 << bit) == 0 {
                Setting::LOW
            } else {
                Setting::HIGH
            }
        };
        let mut spec = reference::spec(species);
        spec.build = Build {
            height: end(0),
            girth: end(1),
            taper: end(2),
            limbs: end(3),
            head: end(4),
        };
        Identity::new(spec).expect("a build setting is never out of range")
    })
}
