use super::{Places, MAX_LANDMARKS, MAX_SITES};
use crate::geom::CellCoord;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::RealmField;

fn places(seed: u64) -> (RealmField, Places) {
    let params = RealmParams::new(RealmSpec {
        seed,
        extent_chunks: 32,
        coarse_samples: 64,
        ..RealmParams::winter_default(seed).spec()
    })
    .expect("legal");
    let field = RealmField::generate(params).expect("solves");
    let places = Places {
        sites: field.sites().to_vec(),
        roads: field.roads().to_vec(),
        landmarks: field.landmarks().to_vec(),
    };
    (field, places)
}

#[test]
fn the_counts_respect_their_bounds() {
    let (_, places) = places(0x517E);
    assert!(!places.sites.is_empty(), "a realm has somewhere to live");
    assert!(places.sites.len() <= MAX_SITES);
    assert!(places.landmarks.len() <= MAX_LANDMARKS);
    assert!(
        places.roads.len() < places.sites.len(),
        "a tree, not a mesh"
    );
}

#[test]
fn no_settlement_stands_in_water() {
    let (field, places) = places(0x517E);
    for site in &places.sites {
        let (gx, gy) = field.grid_position(site.at);
        let sample = field.sample(
            tairix_util::mathf::round_i32(gx),
            tairix_util::mathf::round_i32(gy),
        );
        assert!(!sample.is_water(), "a settlement was placed on water");
        assert!(site.radius_cells >= 6);
    }
}

#[test]
fn settlements_keep_their_distance() {
    let (field, places) = places(0x517E);
    let step = i64::from(field.params().cells_per_coarse());
    let separation = i64::from(field.side() / 12).max(3) * step;
    for (index, a) in places.sites.iter().enumerate() {
        for b in &places.sites[index + 1..] {
            let dx = i64::from(a.at.x - b.at.x).abs();
            let dy = i64::from(a.at.y - b.at.y).abs();
            assert!(dx.max(dy) >= separation, "two settlements overlap");
        }
    }
}

#[test]
fn every_road_connects_the_settlements_it_claims_to() {
    let (field, places) = places(0x517E);
    let step = i64::from(field.params().cells_per_coarse());
    for road in &places.roads {
        assert!(!road.path.is_empty(), "a road with no path");
        let from = places.sites[usize::from(road.from)].at;
        let to = places.sites[usize::from(road.to)].at;
        let near = |a: CellCoord, b: CellCoord| {
            i64::from(a.x - b.x).abs().max(i64::from(a.y - b.y).abs()) <= step
        };
        let start = road.path[0];
        let end = road.path[road.path.len() - 1];
        assert!(
            near(start, from) && near(end, to),
            "a road's ends are not its settlements"
        );
    }
}

#[test]
fn a_road_is_a_connected_walk_of_single_steps() {
    let (field, places) = places(0x517E);
    let step = i64::from(field.params().cells_per_coarse());
    for road in &places.roads {
        for pair in road.path.windows(2) {
            let dx = i64::from(pair[1].x - pair[0].x).abs();
            let dy = i64::from(pair[1].y - pair[0].y).abs();
            assert!(dx <= step && dy <= step && (dx > 0 || dy > 0));
        }
    }
}

#[test]
fn a_road_never_doubles_back_on_itself() {
    let (_, places) = places(0x517E);
    for road in &places.roads {
        let mut seen = alloc::vec::Vec::new();
        for cell in &road.path {
            assert!(!seen.contains(cell), "a least-cost path revisited a cell");
            seen.push(*cell);
        }
    }
}

#[test]
fn every_site_index_a_road_names_exists() {
    let (_, places) = places(0x517E);
    for road in &places.roads {
        assert!(usize::from(road.from) < places.sites.len());
        assert!(usize::from(road.to) < places.sites.len());
        assert_ne!(road.from, road.to);
    }
}

#[test]
fn landmarks_avoid_settlements_and_each_other() {
    let (field, places) = places(0x517E);
    let step = i64::from(field.params().cells_per_coarse());
    let separation = i64::from(field.side() / 24).max(2) * step;
    for (index, a) in places.landmarks.iter().enumerate() {
        for b in &places.landmarks[index + 1..] {
            let dx = i64::from(a.at.x - b.at.x).abs();
            let dy = i64::from(a.at.y - b.at.y).abs();
            assert!(dx.max(dy) >= separation, "two landmarks overlap");
        }
        for site in &places.sites {
            let dx = i64::from(a.at.x - site.at.x).abs();
            let dy = i64::from(a.at.y - site.at.y).abs();
            assert!(
                dx.max(dy) >= i64::from(site.radius_cells) + step,
                "a landmark stands inside a settlement"
            );
        }
    }
}

#[test]
fn a_realm_with_no_land_places_nothing_and_does_not_fail() {
    let params = RealmParams::new(RealmSpec {
        seed: 4,
        extent_chunks: 8,
        coarse_samples: 32,
        ocean_permille: 1000,
        ..RealmParams::winter_default(4).spec()
    })
    .expect("legal");
    let field = RealmField::generate(params).expect("a water world still solves");
    assert!(field.sites().is_empty());
    assert!(field.roads().is_empty());
}
