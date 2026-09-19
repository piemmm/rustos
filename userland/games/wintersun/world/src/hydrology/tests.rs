use alloc::vec::Vec;

use super::{solve, FlowDir, NEIGHBOURS};
use crate::params::{RealmParams, RealmSpec};
use crate::realm::{try_filled, CoarseSample};
use crate::relief;
use crate::seed::SeedKey;
use crate::uplift::Plates;

fn solved(seed: u64, samples_side: u32) -> (RealmParams, Vec<CoarseSample>) {
    let spec = RealmSpec {
        seed,
        extent_chunks: 32,
        coarse_samples: samples_side,
        ..RealmParams::winter_default(seed).spec()
    };
    let params = RealmParams::new(spec).expect("legal");
    let side = params.coarse_samples() as usize;
    let mut samples = try_filled(side * side, CoarseSample::default()).expect("fits");
    relief::solve(
        params,
        SeedKey::new(params.seed()),
        Plates::new(params),
        &mut samples,
    )
    .expect("solves");
    solve(params, &mut samples).expect("solves");
    (params, samples)
}

fn downstream(samples: &[CoarseSample], index: usize, side: u32) -> Option<usize> {
    let width = side as usize;
    let sx = u32::try_from(index % width).expect("in grid");
    let sy = u32::try_from(index / width).expect("in grid");
    let (dx, dy) = samples[index].flow.offset()?;
    let nx = sx.checked_add_signed(dx)?;
    let ny = sy.checked_add_signed(dy)?;
    (nx < side && ny < side).then(|| (ny as usize) * width + (nx as usize))
}

#[test]
fn every_flow_direction_offsets_by_one_cell() {
    for (dir, dx, dy, distance) in NEIGHBOURS {
        assert_eq!(dir.offset(), Some((dx, dy)));
        assert!(dx.abs() <= 1 && dy.abs() <= 1);
        let expected = if dx == 0 || dy == 0 {
            1.0
        } else {
            core::f64::consts::SQRT_2
        };
        assert!((distance - expected).abs() < f64::EPSILON);
    }
    assert_eq!(FlowDir::Sink.offset(), None);
}

#[test]
fn no_sample_drains_to_itself() {
    let (params, samples) = solved(0x5EA, 64);
    let side = params.coarse_samples();
    for index in 0..samples.len() {
        assert_ne!(downstream(&samples, index, side), Some(index));
    }
}

#[test]
fn the_drainage_network_is_acyclic_and_reaches_an_outlet() {
    let (params, samples) = solved(0x5EA, 64);
    let side = params.coarse_samples();
    // Following the network from anywhere must terminate. A cycle would
    // run past the cell count; there is no unbounded retry here.
    for start in 0..samples.len() {
        let mut here = start;
        for _ in 0..=samples.len() {
            match downstream(&samples, here, side) {
                Some(next) => here = next,
                None => break,
            }
        }
        assert!(
            downstream(&samples, here, side).is_none(),
            "a flow path from {start} did not reach a sink"
        );
    }
}

#[test]
fn water_never_flows_uphill() {
    let (params, samples) = solved(0xD12, 64);
    let side = params.coarse_samples();
    for index in 0..samples.len() {
        let Some(next) = downstream(&samples, index, side) else {
            continue;
        };
        // The *water* surface is the monotone one: a lake's cells share a
        // surface, so the ground beneath them may rise toward the outflow
        // while the water on top of it does not.
        assert!(
            samples[next].water.0 <= samples[index].water.0,
            "sample {index} drains uphill"
        );
    }
}

#[test]
fn discharge_grows_strictly_downstream() {
    let (params, samples) = solved(0xD12, 64);
    let side = params.coarse_samples();
    for index in 0..samples.len() {
        assert!(samples[index].discharge >= 1, "every cell drains itself");
        let Some(next) = downstream(&samples, index, side) else {
            continue;
        };
        assert!(
            samples[next].discharge > samples[index].discharge,
            "a channel narrowed on its way downstream"
        );
    }
}

#[test]
fn a_lake_stands_above_the_ground_it_covers() {
    let (_, samples) = solved(0x1A4E, 64);
    let mut lakes = 0;
    for sample in &samples {
        assert!(
            sample.water >= sample.elevation || sample.elevation.is_submerged(),
            "a water surface below its own bed"
        );
        if sample.water > sample.elevation && !sample.elevation.is_submerged() {
            lakes += 1;
        }
    }
    assert!(lakes > 0, "erosion over a plate field leaves basins");
}

#[test]
fn the_rim_and_the_sea_drain_out_of_the_model() {
    let (params, samples) = solved(0x5EA, 64);
    let side = params.coarse_samples();
    for sy in 0..side {
        for sx in 0..side {
            let index = (sy * side + sx) as usize;
            let rim = sx == 0 || sy == 0 || sx + 1 == side || sy + 1 == side;
            if rim {
                assert_eq!(samples[index].flow, FlowDir::Sink);
            }
        }
    }
}

#[test]
fn the_solve_is_a_pure_function_of_its_input() {
    let (_, first) = solved(0x99, 64);
    let (_, second) = solved(0x99, 64);
    assert_eq!(first, second);
}
