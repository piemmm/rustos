use super::{
    budget, params, Particle, ParticleField, ParticleKind, Spawn, AREA_PER_PARTICLE, MAX_PARTICLES,
};
use tairix_reclaim::PressureBand;
use tairix_wintersun_net::value::{WorldPoint, WorldVector};

const SEED: u64 = 0x534E_4F57_4641_4C4C;

fn at(x: i32, y: i32) -> WorldPoint {
    WorldPoint { x, y }
}

fn still() -> WorldVector {
    WorldVector { x: 0, y: 0 }
}

fn field(count: usize) -> ParticleField {
    ParticleField::with_budget(count).expect("a test budget fits")
}

#[test]
fn every_kind_has_plausible_parameters() {
    for kind in ParticleKind::ALL {
        let p = params(kind);
        assert!(p.life > 0, "{kind:?} dies before it is drawn");
        assert!(p.radius > 0, "{kind:?} has no size");
        assert!(p.opacity > 0, "{kind:?} is invisible");
        assert!(p.drag > 0, "{kind:?} stops dead every tick");
    }
}

#[test]
fn the_kinds_that_should_rise_do() {
    assert!(params(ParticleKind::Ember).gravity < 0);
    assert!(params(ParticleKind::Smoke).gravity < 0);
    assert!(params(ParticleKind::Hail).gravity > params(ParticleKind::Snow).gravity);
    // Snow is the wind's more than gravity's, hail the opposite.
    assert!(params(ParticleKind::Snow).drift > params(ParticleKind::Hail).drift);
}

#[test]
fn a_budget_scales_with_the_area_on_screen() {
    let small = budget(AREA_PER_PARTICLE * 100, PressureBand::Normal);
    let large = budget(AREA_PER_PARTICLE * 400, PressureBand::Normal);
    assert_eq!(small, 100);
    assert_eq!(large, 400);
}

#[test]
fn a_budget_tightens_as_the_band_deepens() {
    let area = AREA_PER_PARTICLE * 1024;
    let mut previous = usize::MAX;
    for band in PressureBand::ALL {
        let now = budget(area, band);
        assert!(now < previous, "{band:?} earns {now} against {previous}");
        previous = now;
    }
    assert_eq!(budget(area, PressureBand::Critical), 0);
}

#[test]
fn an_enormous_view_is_still_bounded() {
    // A camera pulled back over a whole realm must not turn weather into
    // an allocation path.
    assert_eq!(budget(u64::MAX, PressureBand::Normal), MAX_PARTICLES);
    let field = field(usize::MAX);
    assert_eq!(field.budget(), MAX_PARTICLES);
}

#[test]
fn a_field_budgeted_for_nothing_admits_nothing() {
    let spawn = Spawn::new(SEED);
    let mut field = field(0);
    assert!(!field.emit(&spawn, ParticleKind::Snow, at(0, 0), still()));
    assert!(field.is_empty());
}

#[test]
fn emitting_fills_up_to_the_budget_and_then_retires_the_oldest() {
    let spawn = Spawn::new(SEED);
    let mut field = field(4);
    for i in 0..4 {
        assert!(field.emit(&spawn, ParticleKind::Rain, at(i, 0), still()));
    }
    assert_eq!(field.len(), 4);
    assert_eq!(field.get(0).expect("held").at, at(0, 0));

    assert!(field.emit(&spawn, ParticleKind::Rain, at(99, 0), still()));
    assert_eq!(field.len(), 4);
    assert_eq!(
        field.get(0).expect("held").at,
        at(1, 0),
        "the oldest was not retired"
    );
    assert_eq!(field.get(3).expect("held").at, at(99, 0));
}

#[test]
fn rebudgeting_down_drops_the_oldest() {
    let spawn = Spawn::new(SEED);
    let mut field = field(8);
    for i in 0..8 {
        field.emit(&spawn, ParticleKind::Dust, at(i, 0), still());
    }
    field.rebudget(3);
    assert_eq!(field.len(), 3);
    assert_eq!(field.get(0).expect("held").at, at(5, 0));
    assert_eq!(field.budget(), 3);
}

#[test]
fn rebudgeting_to_nothing_empties_the_field() {
    let spawn = Spawn::new(SEED);
    let mut field = field(4);
    field.emit(&spawn, ParticleKind::Leaf, at(0, 0), still());
    field.rebudget(0);
    assert!(field.is_empty());
    assert!(!field.emit(&spawn, ParticleKind::Leaf, at(0, 0), still()));
}

#[test]
fn a_particle_expires_exactly_at_its_life() {
    let spawn = Spawn::new(SEED);
    let life = params(ParticleKind::Spark).life;
    let mut field = field(2);
    field.emit(&spawn, ParticleKind::Spark, at(0, 0), still());

    for tick in 1..life {
        field.advance(still());
        assert_eq!(field.len(), 1, "gone early at tick {tick}");
    }
    field.advance(still());
    assert!(field.is_empty(), "outlived its life");
}

#[test]
fn gravity_pulls_a_falling_particle_south() {
    let spawn = Spawn::new(SEED);
    let mut field = field(2);
    field.emit(&spawn, ParticleKind::Hail, at(0, 0), still());
    field.advance(still());
    assert!(field.get(0).expect("held").at.y > 0);
    assert_eq!(field.get(0).expect("held").at.x, 0);
}

#[test]
fn an_ember_rises() {
    let spawn = Spawn::new(SEED);
    let mut field = field(2);
    field.emit(&spawn, ParticleKind::Ember, at(0, 0), still());
    field.advance(still());
    assert!(field.get(0).expect("held").at.y < 0);
}

#[test]
fn the_wind_carries_a_particle() {
    let spawn = Spawn::new(SEED);
    let wind = WorldVector { x: 300, y: 0 };
    let mut field = field(2);
    field.emit(&spawn, ParticleKind::Snow, at(0, 0), still());
    for _ in 0..8 {
        field.advance(wind);
    }
    assert!(field.get(0).expect("held").at.x > 0, "the wind did nothing");
}

#[test]
fn drift_decides_how_much_the_wind_matters() {
    let spawn = Spawn::new(SEED);
    let wind = WorldVector { x: 400, y: 0 };
    let travel = |kind| {
        let mut field = field(2);
        field.emit(&spawn, kind, at(0, 0), still());
        for _ in 0..10 {
            field.advance(wind);
        }
        field.get(0).map_or(0, |p| p.at.x)
    };
    assert!(
        travel(ParticleKind::Snow) > travel(ParticleKind::Hail),
        "hail is blown as far as snow",
    );
}

#[test]
fn a_particle_survives_an_absurd_wind_without_overflowing() {
    let spawn = Spawn::new(SEED);
    let gale = WorldVector {
        x: i16::MAX,
        y: i16::MIN,
    };
    let mut field = field(2);
    field.emit(
        &spawn,
        ParticleKind::Smoke,
        at(i32::MAX - 1, i32::MIN + 1),
        WorldVector {
            x: i16::MAX,
            y: i16::MIN,
        },
    );
    for _ in 0..64 {
        field.advance(gale);
    }
    // Saturating, not wrapping: a particle pinned at the edge of the
    // world is harmless, one that teleported to the far side is not.
    if let Some(p) = field.get(0) {
        assert_eq!(p.at.x, i32::MAX);
        assert_eq!(p.at.y, i32::MIN);
    }
}

#[test]
fn a_particle_fades_only_over_its_tail() {
    let spawn = Spawn::new(SEED);
    let mut field = field(2);
    field.emit(&spawn, ParticleKind::Smoke, at(0, 0), still());
    let full = field.get(0).expect("held").color().a;
    assert_eq!(full, params(ParticleKind::Smoke).opacity);

    let life = params(ParticleKind::Smoke).life;
    let mut alpha = full;
    for _ in 0..life - 1 {
        field.advance(still());
        let now = field.get(0).expect("held").color().a;
        assert!(now <= alpha, "a particle brightened as it aged");
        alpha = now;
    }
    assert!(alpha < full / 4, "it never faded: {alpha} of {full}");
}

#[test]
fn spent_runs_from_nothing_to_everything() {
    let fresh = Particle {
        kind: ParticleKind::Rain,
        at: at(0, 0),
        velocity: still(),
        age: 0,
        variation: 0,
    };
    assert_eq!(fresh.spent(), 0);
    assert!(!fresh.expired());

    let done = Particle {
        age: params(ParticleKind::Rain).life,
        ..fresh
    };
    assert_eq!(done.spent(), 255);
    assert!(done.expired());
}

#[test]
fn particles_of_one_kind_do_not_all_look_alike() {
    let spawn = Spawn::new(SEED);
    let mut field = field(64);
    for i in 0..64 {
        field.emit(&spawn, ParticleKind::Snow, at(i * 640, i * 71), still());
    }
    let first = field.get(0).expect("held").variation;
    assert!(
        field.particles().any(|p| p.variation != first),
        "every flake is identical",
    );
    let radius = field.get(0).expect("held").radius();
    assert!(field.particles().any(|p| p.radius() != radius));
}

#[test]
fn two_particles_at_one_point_still_differ() {
    // Emitting a burst from a single source must not produce a stack of
    // identical particles.
    let spawn = Spawn::new(SEED);
    let mut field = field(16);
    for _ in 0..16 {
        field.emit(&spawn, ParticleKind::Spark, at(500, 500), still());
    }
    let first = field.get(0).expect("held").variation;
    assert!(field.particles().any(|p| p.variation != first));
}

#[test]
fn two_realms_vary_their_particles_differently() {
    let mut a = field(8);
    let mut b = field(8);
    let (spawn_a, spawn_b) = (Spawn::new(SEED), Spawn::new(SEED ^ 0xFF));
    for i in 0..8 {
        a.emit(&spawn_a, ParticleKind::Leaf, at(i * 300, 0), still());
        b.emit(&spawn_b, ParticleKind::Leaf, at(i * 300, 0), still());
    }
    let left: alloc::vec::Vec<u8> = a.particles().map(|p| p.variation).collect();
    let right: alloc::vec::Vec<u8> = b.particles().map(|p| p.variation).collect();
    assert_ne!(left, right);
}

#[test]
fn a_field_advances_reproducibly() {
    let spawn = Spawn::new(SEED);
    let run = || {
        let mut field = field(32);
        for i in 0..32 {
            field.emit(&spawn, ParticleKind::Sleet, at(i * 128, i * 64), still());
        }
        for tick in 0..20 {
            field.advance(WorldVector {
                x: 120 - tick * 8,
                y: 30,
            });
        }
        field.particles().copied().collect::<alloc::vec::Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn clearing_empties_but_keeps_the_budget() {
    let spawn = Spawn::new(SEED);
    let mut field = field(6);
    field.emit(&spawn, ParticleKind::Splash, at(0, 0), still());
    field.clear();
    assert!(field.is_empty());
    assert_eq!(field.budget(), 6);
    assert!(field.emit(&spawn, ParticleKind::Splash, at(0, 0), still()));
}
