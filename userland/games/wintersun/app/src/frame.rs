//! One frame, from the ground up.
//!
//! The pass order is fixed: terrain splat, then the light and fog
//! composite over it. Ground scenery, entities, canopy, particles and
//! weather take their places between the two as later items add them,
//! which is why the budget names them now and the renderer measures
//! every pass rather than only the ones that currently do work.
//!
//! # Ask, then paint
//!
//! Everything a band needs is resolved before any band starts: the
//! chunks are already resident, the lattice is sampled, and the material
//! tiles are made resident by the one mutable pass over the cache. The
//! bands then only read. That is what lets them run on other cores at
//! all, and it is the same discipline that keeps a frame off the
//! filesystem: a paint reads nothing it has to wait for.
//!
//! # What a band is
//!
//! A full-width run of rows, claimed dynamically. Every pass steps
//! horizontally, so a vertical cut would divide the span each is built
//! around; the bands are cut finer than the runner is wide so a core
//! taken by another tenant holds up one small piece rather than a whole
//! share of the frame.

use alloc::vec::Vec;

use tairix_parallel::{for_each, JobRunner};
use tairix_raster::color::Pixel;
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::{Decal, Fray};
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::chunk::ChunkWindow;

use crate::budget::{FrameTimes, Pass};
use crate::camera::Camera;
use crate::error::ClientError;
use crate::light::{LightBuffer, Lit, Shading, Sky, Sun};
use crate::quality::Ladder;
use crate::terrain::{self, TerrainGrid};
use crate::view::Viewport;

/// Where the renderer reads the time from.
///
/// Injected because the crate is `no_std` and asks the kernel nothing,
/// and because the per-pass measurement is the milestone's own exit
/// criterion: a host test drives it with a clock it controls, and the
/// real client passes the monotonic one.
pub trait Clock {
    /// Monotonic nanoseconds. Only differences are read.
    fn now_ns(&self) -> u64;
}

/// A clock that never advances.
///
/// For a caller that wants a frame and not a measurement — the digest
/// vertical, most of all, whose answer must not depend on how long the
/// guest took to produce it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Stopped;

impl Clock for Stopped {
    fn now_ns(&self) -> u64 {
        0
    }
}

/// Everything outside the renderer that one frame depends on.
#[derive(Copy, Clone)]
pub struct Scene<'a> {
    /// Where the player is looking.
    pub camera: Camera,
    /// The chunks the client holds.
    pub chunks: ChunkWindow<'a>,
    /// The roads and rivers stamped into the ground.
    pub decals: &'a [Decal<'a>],
    /// The realm's frayed decal edges.
    pub fray: &'a Fray,
    /// The realm's material warp.
    pub warp: &'a Warp,
    /// The light.
    pub sun: Sun,
    /// What the ground sits under.
    pub sky: Sky,
    /// How far the renderer has fallen back.
    pub ladder: Ladder,
}

/// The reusable buffers one client's frames are drawn through.
#[derive(Debug, Default)]
pub struct Renderer {
    grid: TerrainGrid,
    light: LightBuffer,
    scratch: Vec<Vec<Lit>>,
}

/// One band's share of a frame.
struct BandWork<'a> {
    first_row: u32,
    pixels: &'a mut [Pixel],
    scratch: &'a mut Vec<Lit>,
}

impl Renderer {
    /// A renderer with nothing yet allocated.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The lattice the last frame was drawn from.
    #[must_use]
    pub const fn grid(&self) -> &TerrainGrid {
        &self.grid
    }

    /// The light the last frame was lit by.
    #[must_use]
    pub const fn light(&self) -> &LightBuffer {
        &self.light
    }

    /// Draw one frame into `target`, which must hold the view's render
    /// extent.
    ///
    /// # Errors
    ///
    /// [`ClientError::Viewport`] when `target` is not the render extent,
    /// and [`ClientError::OutOfMemory`] when a buffer the frame needs
    /// does not fit.
    pub fn render(
        &mut self,
        target: &mut [Pixel],
        view: &Viewport,
        scene: &Scene<'_>,
        cache: &mut MaterialCache,
        runner: &dyn JobRunner,
        clock: &dyn Clock,
    ) -> Result<FrameTimes, ClientError> {
        if target.len() != view.render_pixels() {
            return Err(ClientError::Viewport);
        }
        let (width, height) = view.render();
        let step = scene.camera.zoom().sub_units_per_pixel();
        let origin = scene.camera.origin(width, height);
        let mut times = FrameTimes::new();

        let started = clock.now_ns();
        self.grid.rebuild(
            &scene.chunks,
            scene.camera.visible(width, height),
            scene.decals,
            scene.fray,
        )?;
        let quality = scene.ladder.material_quality();
        terrain::ensure_tiles(cache, &self.grid, quality, step);

        let pass = terrain::Pass {
            warp: scene.warp,
            cache,
            quality,
            step,
            origin,
        };
        let count = view.band_count(runner);
        ensure_scratch(&mut self.scratch, count, 0)?;
        let grid = &self.grid;
        dispatch(target, &mut self.scratch, view, runner, width, &|band| {
            for row in 0..rows_of(band, width) {
                let start = (row as usize) * (width as usize);
                let end = start + (width as usize);
                terrain::paint_row(
                    &mut band.pixels[start..end],
                    grid,
                    &pass,
                    band.first_row + row,
                );
            }
        })?;
        times.record(Pass::Terrain, clock.now_ns().saturating_sub(started));

        let lit = clock.now_ns();
        let shading = Shading {
            sun: scene.sun,
            shift: scene.ladder.light_shift(),
            shadow: scene.ladder.shadow(),
            step,
            origin,
        };
        self.light.shade(view, &self.grid, &shading, runner)?;
        ensure_scratch(&mut self.scratch, count, self.light.scratch_len())?;
        let light = &self.light;
        let sky = scene.sky;
        dispatch(target, &mut self.scratch, view, runner, width, &|band| {
            for row in 0..rows_of(band, width) {
                let start = (row as usize) * (width as usize);
                let end = start + (width as usize);
                light.composite_row(
                    &mut band.pixels[start..end],
                    band.scratch,
                    sky,
                    band.first_row + row,
                );
            }
        })?;
        times.record(Pass::Light, clock.now_ns().saturating_sub(lit));
        Ok(times)
    }
}

/// Make sure there is one scratch row of `width` texels per band.
fn ensure_scratch(
    scratch: &mut Vec<Vec<Lit>>,
    bands: usize,
    width: usize,
) -> Result<(), ClientError> {
    if scratch.len() < bands {
        scratch
            .try_reserve(bands - scratch.len())
            .map_err(|_| ClientError::OutOfMemory)?;
        scratch.resize_with(bands, Vec::new);
    }
    for row in scratch.iter_mut().take(bands) {
        if row.len() < width {
            row.try_reserve(width - row.len())
                .map_err(|_| ClientError::OutOfMemory)?;
            row.resize(width, Lit::NEUTRAL);
        }
    }
    Ok(())
}

/// Cut `target` into bands and run `visit` over them.
///
/// A free function rather than a method so the lattice and the light the
/// bands read can be borrowed from the renderer at the same time as the
/// scratch rows they write.
fn dispatch(
    target: &mut [Pixel],
    scratch: &mut [Vec<Lit>],
    view: &Viewport,
    runner: &dyn JobRunner,
    width: u32,
    visit: &(dyn Fn(&mut BandWork<'_>) + Sync),
) -> Result<(), ClientError> {
    let count = view.band_count(runner).min(scratch.len());
    let rows = view.band_rows(view.band_count(runner));
    let mut work: Vec<BandWork<'_>> = Vec::new();
    work.try_reserve(count)
        .map_err(|_| ClientError::OutOfMemory)?;

    let mut rest = target;
    let mut scratches = scratch;
    for index in 0..count {
        let Some((start, end)) = rows.range(index) else {
            break;
        };
        let take = (end - start) * (width as usize);
        let (mine, tail) = rest.split_at_mut(take.min(rest.len()));
        rest = tail;
        let Some((scratch, others)) = scratches.split_first_mut() else {
            break;
        };
        scratches = others;
        work.push(BandWork {
            first_row: u32::try_from(start).unwrap_or(u32::MAX),
            pixels: mine,
            scratch,
        });
    }
    for_each(runner, &mut work, visit);
    Ok(())
}

/// How many rows a band covers.
fn rows_of(band: &BandWork<'_>, width: u32) -> u32 {
    let rows = band.pixels.len() / (width as usize).max(1);
    u32::try_from(rows).unwrap_or(u32::MAX)
}

/// The world position of a render target's top-left pixel.
///
/// The projection's own answer, restated here so a caller assembling a
/// [`Shading`] outside a render does not have to reconstruct it.
#[must_use]
pub fn frame_origin(camera: &Camera, view: &Viewport) -> WorldPoint {
    let (width, height) = view.render();
    camera.origin(width, height)
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
