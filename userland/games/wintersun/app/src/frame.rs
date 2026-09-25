//! One frame, from the ground up.
//!
//! The pass order is fixed: the terrain splat, the light and fog composite
//! over it, then the figures standing on it. The light buffer is the
//! ground's own relief shading, which is not a figure's light, so a figure
//! is drawn after it — shaded from its own surfaces, veiled by the mist at
//! its feet. Canopy, particles, weather and the overlays take their places
//! after as later items add them; the budget names them now, and the
//! renderer measures every pass rather than only the ones that do work.
//!
//! # Ask, then paint
//!
//! Everything a band needs is resolved before any band starts: the chunks
//! are already resident, the lattice is sampled, the material tiles are made
//! resident by the one mutable pass over the cache, and every figure is
//! posed and placed. The bands then only read, which is what lets them run
//! on other cores at all, and it is the same discipline that keeps a frame
//! off the filesystem: a paint reads nothing it has to wait for.
//!
//! # What a band is
//!
//! A full-width run of the target's rows. Every pass steps horizontally, so
//! a vertical cut would divide the span each is built around; the bands are
//! cut finer than the runner is wide so a core taken by another tenant holds
//! up one small piece rather than a whole share of the frame.

use alloc::vec::Vec;

use tairix_parallel::{for_each, JobRunner};
use tairix_raster::surface::{RowBand, Surface};
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::{Decal, Fray};
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_figure::paint::Brush;
use tairix_wintersun_world::chunk::ChunkWindow;

use crate::budget::{FrameTimes, Pass};
use crate::camera::Camera;
use crate::error::ClientError;
use crate::figures::{Cast, Framing, Stage};
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
    /// Everyone standing in it.
    pub cast: &'a Cast<'a>,
}

/// The reusable buffers one client's frames are drawn through.
#[derive(Debug, Default)]
pub struct Renderer {
    grid: TerrainGrid,
    light: LightBuffer,
    tools: Vec<Tools>,
    stage: Stage,
}

/// What one band paints with, held across frames so a band allocates
/// nothing once the frame has been drawn at its size.
#[derive(Debug, Default)]
struct Tools {
    scratch: Vec<Lit>,
    brush: Brush,
}

/// One band's share of a frame.
struct BandWork<'a> {
    band: RowBand<'a>,
    tools: &'a mut Tools,
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

    /// How many figures the last frame drew.
    #[must_use]
    pub fn figures(&self) -> usize {
        self.stage.placed()
    }

    /// Draw one frame of `scene` into `target`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Viewport`] when `target` is not the view's render
    /// extent or a clip window cuts it, [`ClientError::OutOfMemory`] when a
    /// buffer the frame needs does not fit, and [`ClientError::Figure`] for
    /// a sun no figure can be lit by.
    pub fn render(
        &mut self,
        target: &mut Surface,
        view: &Viewport,
        scene: &Scene<'_>,
        cache: &mut MaterialCache,
        runner: &dyn JobRunner,
        clock: &dyn Clock,
    ) -> Result<FrameTimes, ClientError> {
        let (width, height) = view.render();
        if !whole(target, width, height) {
            return Err(ClientError::Viewport);
        }
        let step = scene.camera.step(view);
        let origin = scene.camera.origin(view);
        let visible = scene.camera.visible(view);
        let rows = view.band_rows(runner);
        let mut times = FrameTimes::new();

        let started = clock.now_ns();
        self.grid
            .rebuild(&scene.chunks, visible, scene.decals, scene.fray)?;
        let quality = scene.ladder.material_quality();
        terrain::ensure_tiles(cache, &self.grid, quality, step);
        let pass = terrain::Pass {
            warp: scene.warp,
            cache,
            quality,
            step,
            origin,
        };
        ensure_tools(&mut self.tools, height.div_ceil(rows), 0)?;
        let grid = &self.grid;
        dispatch(target, &mut self.tools, rows, runner, &|work| {
            for row in work.band.rows() {
                if let Some((_, pixels)) = work.band.row_span_mut(row, 0, width) {
                    terrain::paint_row(pixels, grid, &pass, row);
                }
            }
        })?;
        times.record(Pass::Terrain, clock.now_ns().saturating_sub(started));

        let lit = clock.now_ns();
        let shading = Shading {
            sun: scene.sun,
            shift: scene.ladder.light_shift(),
            relief: scene.ladder.relief(),
            step,
            origin,
        };
        self.light.shade(view, &self.grid, &shading, runner)?;
        ensure_tools(
            &mut self.tools,
            height.div_ceil(rows),
            self.light.scratch_len(),
        )?;
        let light = &self.light;
        let sky = scene.sky;
        dispatch(target, &mut self.tools, rows, runner, &|work| {
            for row in work.band.rows() {
                if let Some((_, pixels)) = work.band.row_span_mut(row, 0, width) {
                    light.composite_row(pixels, &mut work.tools.scratch, sky, row);
                }
            }
        })?;
        times.record(Pass::Light, clock.now_ns().saturating_sub(lit));

        let staged = clock.now_ns();
        let framing = Framing {
            origin,
            step,
            visible,
            light: scene.sun.light()?,
            shade: scene.ladder.shadow(),
        };
        self.stage
            .place(scene.cast, &framing, &self.light, sky, runner)?;
        if self.stage.placed() > 0 {
            let stage = &self.stage;
            dispatch(target, &mut self.tools, rows, runner, &|work| {
                stage.paint(&mut work.band, &mut work.tools.brush);
            })?;
        }
        times.record(Pass::Scenery, clock.now_ns().saturating_sub(staged));
        Ok(times)
    }
}

/// Whether `target` admits every pixel of a `width` × `height` frame: it is
/// that size, and no clip window or origin cuts any of it.
///
/// Every pass paints whole rows from the target's left edge, so a target it
/// could reach only part of is refused rather than drawn wrongly.
fn whole(target: &mut Surface, width: u32, height: u32) -> bool {
    if target.width() != width || target.height() != height {
        return false;
    }
    let Some(mut band) = target.row_bands_mut(0..height, height).next() else {
        return false;
    };
    band.rows() == (0..height)
        && band
            .row_span_mut(0, 0, width)
            .is_some_and(|(first, span)| first == 0 && span.len() == width as usize)
}

/// Make sure there is a band's tools for each of `bands` bands, each with a
/// scratch row of at least `width` texels.
fn ensure_tools(tools: &mut Vec<Tools>, bands: u32, width: usize) -> Result<(), ClientError> {
    let bands = usize::try_from(bands).map_err(|_| ClientError::OutOfMemory)?;
    if tools.len() < bands {
        tools
            .try_reserve(bands - tools.len())
            .map_err(|_| ClientError::OutOfMemory)?;
        tools.resize_with(bands, Tools::default);
    }
    for band in tools.iter_mut().take(bands) {
        if band.scratch.len() < width {
            band.scratch
                .try_reserve(width - band.scratch.len())
                .map_err(|_| ClientError::OutOfMemory)?;
            band.scratch.resize(width, Lit::NEUTRAL);
        }
    }
    Ok(())
}

/// Cut `target` into bands of `rows` rows and run `visit` over them, each
/// with its own tools.
///
/// A free function rather than a method so the lattice, the light and the
/// placed figures the bands read can be borrowed from the renderer at the
/// same time as the tools they write. The caller has made sure there are
/// tools for every band.
fn dispatch(
    target: &mut Surface,
    tools: &mut [Tools],
    rows: u32,
    runner: &dyn JobRunner,
    visit: &(dyn Fn(&mut BandWork<'_>) + Sync),
) -> Result<(), ClientError> {
    let bands = target.row_bands_mut(0..target.height(), rows);
    let mut work: Vec<BandWork<'_>> = Vec::new();
    work.try_reserve(bands.len())
        .map_err(|_| ClientError::OutOfMemory)?;
    for (band, tools) in bands.zip(tools.iter_mut()) {
        work.push(BandWork { band, tools });
    }
    for_each(runner, &mut work, visit);
    Ok(())
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
