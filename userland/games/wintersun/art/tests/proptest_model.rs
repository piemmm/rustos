//! Stateful property model for `WinterSun`'s ground art.
//!
//! Everything the splat draws rests on one invariant: a weight field is a
//! normalised vector of at most four distinct materials, ordered heaviest
//! first, with no empty slot. Break it anywhere and the splat divides by a
//! sum it was not given, or reads a texture for a material contributing
//! nothing, or draws the same material twice. So a randomised program of
//! legal operations — covering, interpolating, stamping decals,
//! re-budgeting and advancing a particle field, and drawing spans — is
//! replayed, and the invariant is checked after **every** command.
//!
//! Two things make it sharp. The workspace builds with overflow checks on
//! in every profile, so arithmetic that wrapped aborts the case rather
//! than quietly producing a wrong number — and this crate's arithmetic is
//! full of products of weights, coverages and distances. And the spans are
//! actually drawn, so a field that passed the invariant but that the
//! kernel could not consume would still be caught.
//!
//! Unlike a fuzz harness, which hammers bytes looking for crashes, this
//! generates *structured* programs and lets proptest shrink a
//! counterexample to a minimal failing one. This crate decodes nothing
//! untrusted, so it has no decoder for a fuzzer to point at; the
//! adversarial coverage it needs is this.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tairix_raster::color::Pixel;
use tairix_reclaim::PressureBand;
use tairix_wintersun_art::decal::{Decal, Fray};
use tairix_wintersun_art::material::{MaterialTile, Mip, Quality};
use tairix_wintersun_art::particle::{budget, ParticleField, ParticleKind, Spawn, MAX_PARTICLES};
use tairix_wintersun_art::splat::{splat, Geometry, SpanPlan, SpanTiles, Warp};
use tairix_wintersun_art::weight::{WeightField, TOTAL};
use tairix_wintersun_net::value::{WorldPoint, WorldVector};
use tairix_wintersun_world::biome::{Material, BLEND_SLOTS};

/// Sequences run once by a plain `cargo test` (no budget set).
const SMOKE_CASES: u32 = 96;

/// Sequences per batch under a wall-clock budget.
const BUDGET_BATCH_CASES: u32 = 192;

/// Commands in one generated program.
const PROGRAM_LEN: usize = 40;

/// The realm every case draws for.
const SEED: u64 = 0x4152_5457_4F52_4B53;

/// Pixels in a drawn span.
const SPAN_PIXELS: usize = 24;

/// One legal thing a caller may do to the art pipeline.
#[derive(Copy, Clone, Debug)]
enum Cmd {
    /// Lay a material over the field at some coverage.
    Cover { material: u8, coverage: u16 },
    /// Interpolate the field toward a solid one.
    Lerp { material: u8, t: u8 },
    /// Replace the field with a solid one.
    Reset { material: u8 },
    /// Stamp a decal at a point.
    Stamp {
        material: u8,
        half_width: u32,
        feather: u32,
        coverage: u16,
        x: i32,
        y: i32,
    },
    /// Draw a span from the field to a solid one.
    Draw { material: u8, x: i32, step: i32 },
    /// Re-budget the particle field.
    Rebudget { area: u64, band: u8 },
    /// Emit a particle.
    Emit {
        kind: u8,
        x: i32,
        y: i32,
        vx: i16,
        vy: i16,
    },
    /// Advance the particle field one tick.
    Advance { wind_x: i16, wind_y: i16 },
}

fn material_of(index: u8) -> Material {
    Material::ALL[usize::from(index) % Material::ALL.len()]
}

fn band_of(index: u8) -> PressureBand {
    PressureBand::ALL[usize::from(index) % PressureBand::ALL.len()]
}

fn command() -> impl Strategy<Value = Cmd> {
    prop_oneof![
        (any::<u8>(), 0u16..400).prop_map(|(material, coverage)| Cmd::Cover { material, coverage }),
        (any::<u8>(), any::<u8>()).prop_map(|(material, t)| Cmd::Lerp { material, t }),
        any::<u8>().prop_map(|material| Cmd::Reset { material }),
        (
            any::<u8>(),
            0u32..4000,
            0u32..4000,
            0u16..400,
            -8000i32..8000,
            -8000i32..8000,
        )
            .prop_map(
                |(material, half_width, feather, coverage, x, y)| Cmd::Stamp {
                    material,
                    half_width,
                    feather,
                    coverage,
                    x,
                    y,
                }
            ),
        (any::<u8>(), any::<i32>(), -4096i32..4096).prop_map(|(material, x, step)| Cmd::Draw {
            material,
            x,
            step
        }),
        (any::<u64>(), any::<u8>()).prop_map(|(area, band)| Cmd::Rebudget { area, band }),
        (
            any::<u8>(),
            any::<i32>(),
            any::<i32>(),
            any::<i16>(),
            any::<i16>()
        )
            .prop_map(|(kind, x, y, vx, vy)| Cmd::Emit { kind, x, y, vx, vy }),
        (any::<i16>(), any::<i16>()).prop_map(|(wind_x, wind_y)| Cmd::Advance { wind_x, wind_y }),
    ]
}

fn program() -> impl Strategy<Value = Vec<Cmd>> {
    prop::collection::vec(command(), 1..=PROGRAM_LEN)
}

/// The live state one generated program drives.
struct Session {
    field: WeightField,
    particles: ParticleField,
    fray: Fray,
    warp: Warp,
    spawn: Spawn,
    /// One coarse tile, so the drawn spans exercise the tile path rather
    /// than only the flat tier. Coarse because a program draws often and
    /// the model is about invariants, not throughput.
    tile: MaterialTile,
}

impl Session {
    fn new() -> Self {
        Self {
            field: WeightField::solid(Material::Moor),
            particles: ParticleField::with_budget(64).expect("a small budget fits"),
            fray: Fray::new(SEED),
            warp: Warp::new(SEED),
            spawn: Spawn::new(SEED),
            tile: MaterialTile::synthesise(Material::Gravel, Mip::coarsest(), Quality::FULL)
                .expect("a coarse tile fits"),
        }
    }

    fn run(&mut self, command: Cmd) {
        match command {
            Cmd::Cover { material, coverage } => {
                self.field.cover(material_of(material), coverage);
            }
            Cmd::Lerp { material, t } => {
                self.field = self
                    .field
                    .lerp(&WeightField::solid(material_of(material)), t);
            }
            Cmd::Reset { material } => {
                self.field = WeightField::solid(material_of(material));
            }
            Cmd::Stamp {
                material,
                half_width,
                feather,
                coverage,
                x,
                y,
            } => {
                let path = [
                    WorldPoint { x: -5000, y: -500 },
                    WorldPoint { x: 0, y: 0 },
                    WorldPoint { x: 5000, y: 700 },
                ];
                let decal = Decal {
                    material: material_of(material),
                    path: &path,
                    half_width,
                    feather,
                    coverage,
                };
                decal.stamp(&mut self.field, &self.fray, WorldPoint { x, y });
            }
            Cmd::Draw { material, x, step } => {
                let right = WeightField::solid(material_of(material));
                let plan = SpanPlan::new(&self.field, &right);
                let mut tiles: SpanTiles<'_> = [None; BLEND_SLOTS];
                for (slot, planned) in tiles.iter_mut().zip(plan.materials()) {
                    if planned == self.tile.material() {
                        *slot = Some(&self.tile);
                    }
                }
                let origin = WorldPoint { x, y: x ^ 0x5A5A };
                let geometry = Geometry::new(
                    &self.warp,
                    origin,
                    step,
                    u32::try_from(SPAN_PIXELS).unwrap_or(1),
                );
                let mut row = [Pixel::TRANSPARENT; SPAN_PIXELS];
                splat(&mut row, &plan, &tiles, &geometry);
                // Ground is opaque whatever it was drawn from.
                assert!(row.iter().all(|p| p.a == u8::MAX));
            }
            Cmd::Rebudget { area, band } => {
                self.particles.rebudget(budget(area, band_of(band)));
            }
            Cmd::Emit { kind, x, y, vx, vy } => {
                let kind = ParticleKind::ALL[usize::from(kind) % ParticleKind::ALL.len()];
                self.particles.emit(
                    &self.spawn,
                    kind,
                    WorldPoint { x, y },
                    WorldVector { x: vx, y: vy },
                );
            }
            Cmd::Advance { wind_x, wind_y } => {
                self.particles.advance(WorldVector {
                    x: wind_x,
                    y: wind_y,
                });
            }
        }
    }
}

/// Every invariant the art claims to keep, read off the live state.
fn check(session: &Session) -> Result<(), TestCaseError> {
    let slots = session.field.slots();
    prop_assert!(!slots.is_empty(), "a weight field emptied itself");
    prop_assert!(
        slots.len() <= BLEND_SLOTS,
        "a weight field holds {} materials",
        slots.len(),
    );
    prop_assert_eq!(session.field.total(), TOTAL, "a weight field unnormalised");

    let mut previous = u16::MAX;
    for (index, slot) in slots.iter().enumerate() {
        prop_assert!(slot.weight > 0, "slot {index} is empty but counted");
        prop_assert!(slot.weight <= previous, "slot {index} is out of order");
        previous = slot.weight;
        prop_assert!(
            slots[..index].iter().all(|s| s.material != slot.material),
            "{:?} appears twice",
            slot.material,
        );
        prop_assert_eq!(
            session.field.weight_of(slot.material),
            slot.weight,
            "the lookup disagrees with the slot",
        );
    }
    prop_assert_eq!(session.field.dominant(), slots[0].material);

    prop_assert!(
        session.particles.len() <= session.particles.budget(),
        "{} particles against a budget of {}",
        session.particles.len(),
        session.particles.budget(),
    );
    prop_assert!(session.particles.budget() <= MAX_PARTICLES);
    for particle in session.particles.particles() {
        prop_assert!(!particle.expired(), "an expired particle was not reaped");
        prop_assert!(particle.radius() > 0, "a particle has no size");
    }
    Ok(())
}

#[test]
fn no_sequence_of_legal_operations_breaks_an_invariant() {
    tairix_fuzzseed::prop::drive(
        "no_sequence_of_legal_operations_breaks_an_invariant",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        program(),
        |commands| {
            let mut session = Session::new();
            check(&session)?;
            for command in commands {
                session.run(command);
                check(&session)?;
            }
            Ok(())
        },
    );
}

#[test]
fn a_decal_never_stamps_outside_the_bounds_it_reports() {
    // A caller buckets decals per tile by their bounds, so a stamp
    // outside them is simply missed — a road that disappears at a tile
    // edge, found only by looking.
    tairix_fuzzseed::prop::drive(
        "a_decal_never_stamps_outside_the_bounds_it_reports",
        SMOKE_CASES,
        BUDGET_BATCH_CASES,
        (
            any::<u8>(),
            0u32..2000,
            0u32..2000,
            1u16..=TOTAL,
            -6000i32..6000,
            -6000i32..6000,
        ),
        |(material, half_width, feather, coverage, x, y)| {
            let path = [
                WorldPoint { x: -3000, y: -200 },
                WorldPoint { x: 0, y: 0 },
                WorldPoint { x: 3000, y: 400 },
            ];
            let decal = Decal {
                material: material_of(material),
                path: &path,
                half_width,
                feather,
                coverage,
            };
            let fray = Fray::new(SEED);
            let at = WorldPoint { x, y };
            let bounds = decal.bounds().expect("three points");
            if decal.coverage_at(&fray, at) > 0 {
                prop_assert!(
                    bounds.contains(at),
                    "stamped at ({x}, {y}) outside {bounds:?}",
                );
            }
            Ok(())
        },
    );
}
