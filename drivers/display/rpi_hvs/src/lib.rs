//! RustOS Raspberry Pi `VideoCore` HVS hardware-layer display driver.
//!
//! The Raspberry Pi's `VideoCore` Hardware Video Scaler (HVS) is a
//! fixed-function compositor: instead of the CPU blending the whole
//! screen, the HVS reads a *display list* (DLIST) describing a stack of
//! planes and composites them into the scan-out as it drives the
//! display. This driver exposes that engine through the
//! [`AcceleratedDisplay`] seam (`AGENTS.md` §10): the desktop compositor
//! hands it the visible windows as [`AccelLayer`]s and the HVS composites
//! them, so the host never blends the whole screen in software.
//!
//! The driver also implements the plain [`Display`] trait so the
//! software full-frame path stays available as the mandatory fallback
//! (`AGENTS.md` §10) — e.g. when the window stack exceeds the plane
//! budget the HVS can composite in one pass.
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`RpiHvs`] is a public *type* re-exported so the driver host can
//! instantiate it through [`RpiHvs::open`]; the host never reaches into
//! the type beyond the [`Display`] / [`AcceleratedDisplay`] traits.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. Mapping the scan-out
//! framebuffer, the HVS display-list RAM, the per-plane source buffers,
//! and the display-channel control register additionally requires
//! [`CapabilityId::MMIO_MAP`]: every region is device-visible memory
//! reached only through the capability-gated [`MmioMapper`], never
//! through a pointer the driver synthesises itself (`AGENTS.md` §4 — no
//! ambient authority). The driver runs in user space; it does not
//! request `CAP_DRV_KERNEL` (`AGENTS.md` §4 / §8).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, Display, DisplayMode,
};
use rustos_abi::driver::mmio::{MmioMapError, WindowError};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, MmioMapper, RegisterWindow};

#[cfg(test)]
mod tests;

mod dlist;

pub use dlist::{HvsConfig, PlaneConfig, ScanoutConfig, DEFAULT_BUS_ALIAS, MAX_PLANES};

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention the other display drivers use: the bytes
/// spell `"HVS"` with a version nibble.
const REGISTER_HANDLE_MARKER: u64 = 0x4856_5300_0000_0001;

/// Driver entry point (`AGENTS.md` §8).
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// A mapped per-plane source buffer the HVS DMAs pixels from.
struct PlaneBuffer {
    window: RegisterWindow,
    bus_addr: u32,
    len: usize,
}

/// The Raspberry Pi HVS display engine, brought online over the host's
/// [`MmioMapper`].
///
/// Dropping the [`RpiHvs`] drops every [`RegisterWindow`] it owns, which
/// is the driver's quiesce step (the kernel reclaims the mappings on
/// unload, `AGENTS.md` §4). Reloading is calling [`RpiHvs::open`] again.
pub struct RpiHvs {
    mode: DisplayMode,
    scanout: RegisterWindow,
    scanout_len: usize,
    dlist: RegisterWindow,
    dlist_words: usize,
    control: RegisterWindow,
    planes: [Option<PlaneBuffer>; MAX_PLANES],
    plane_count: usize,
    present_generation: u32,
}

impl RpiHvs {
    /// Map the HVS regions described by `config` and bring the engine
    /// online.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `config` describes a
    ///   degenerate surface, a display list too small for one plane, or
    ///   a region whose size overflows the host address width.
    /// * [`DriverError::PermissionDenied`] if the host did not grant
    ///   [`CapabilityId::MMIO_MAP`].
    /// * [`DriverError::Unsupported`] if the host exposes no
    ///   [`MmioMapper`], or the platform cannot map a region.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
    /// [`CapabilityId::DRV_LOAD`] [`register`] checked.
    pub fn open(host: &dyn DriverHost, config: HvsConfig) -> Result<Self, DriverError> {
        config.validate()?;
        if !host.has_capability(CapabilityId::MMIO_MAP) {
            return Err(DriverError::PermissionDenied);
        }
        let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;

        let scanout_len = config.scanout.surface_len()?;
        let scanout = map(mapper, config.scanout.phys_base, scanout_len)?;
        let dlist = map(mapper, config.dlist_phys_base, config.dlist_len_bytes)?;
        let control = map(mapper, config.control_phys_base, dlist::CONTROL_LEN_BYTES)?;

        let mut planes: [Option<PlaneBuffer>; MAX_PLANES] = Default::default();
        for (slot, plane) in planes.iter_mut().zip(config.planes()) {
            let window = map(mapper, plane.phys_base, plane.len_bytes)?;
            *slot = Some(PlaneBuffer {
                window,
                bus_addr: config.bus_address(plane.phys_base)?,
                len: plane.len_bytes,
            });
        }

        Ok(Self {
            mode: config.scanout.mode(),
            scanout,
            scanout_len,
            dlist,
            dlist_words: config.dlist_len_bytes / 4,
            control,
            planes,
            plane_count: config.plane_count,
            present_generation: 0,
        })
    }

    /// Copy `frame[..scanout_len]` into the mapped scan-out window,
    /// word by word with a byte tail (the software fallback path).
    fn blit_scanout(&self, frame: &[u8]) -> Result<(), DriverError> {
        let whole_words = self.scanout_len / 4;
        for word in 0..whole_words {
            let off = word * 4;
            let value =
                u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            self.scanout
                .write_u32(off, value)
                .map_err(WindowError::as_driver_error)?;
        }
        let tail_start = whole_words * 4;
        for (i, &byte) in frame[tail_start..self.scanout_len].iter().enumerate() {
            self.scanout
                .write_u8(tail_start + i, byte)
                .map_err(WindowError::as_driver_error)?;
        }
        Ok(())
    }

    /// Upload one layer's pixels into its plane buffer.
    fn upload_plane(&self, plane: &PlaneBuffer, layer: &AccelLayer<'_>) -> Result<(), DriverError> {
        let need = layer.required_len(self.mode.format)?;
        if layer.pixels.len() < need {
            return Err(DriverError::BufferTooSmall);
        }
        if need > plane.len {
            return Err(DriverError::LengthOutOfRange);
        }
        let whole_words = need / 4;
        for word in 0..whole_words {
            let off = word * 4;
            let value = u32::from_le_bytes([
                layer.pixels[off],
                layer.pixels[off + 1],
                layer.pixels[off + 2],
                layer.pixels[off + 3],
            ]);
            plane
                .window
                .write_u32(off, value)
                .map_err(WindowError::as_driver_error)?;
        }
        let tail_start = whole_words * 4;
        for (i, &byte) in layer.pixels[tail_start..need].iter().enumerate() {
            plane
                .window
                .write_u8(tail_start + i, byte)
                .map_err(WindowError::as_driver_error)?;
        }
        Ok(())
    }
}

/// Map exactly `len` bytes at `phys`, translating the mapper's error.
fn map(mapper: &dyn MmioMapper, phys: u64, len: usize) -> Result<RegisterWindow, DriverError> {
    mapper
        .map_window(phys, len)
        .map_err(MmioMapError::as_driver_error)
}

impl Display for RpiHvs {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        if frame.len() < self.scanout_len {
            return Err(DriverError::BufferTooSmall);
        }
        self.blit_scanout(frame)
    }
}

impl AcceleratedDisplay for RpiHvs {
    fn accel_caps(&self) -> Result<AccelCaps, DriverError> {
        Ok(AccelCaps {
            max_layers: u32::try_from(self.plane_count).unwrap_or(u32::MAX),
            max_width_px: self.mode.width_px,
            max_height_px: self.mode.height_px,
            per_layer_opacity: true,
        })
    }

    fn present_layers(&mut self, layers: &[AccelLayer<'_>]) -> Result<(), DriverError> {
        if layers.len() > self.plane_count {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut builder = dlist::DlistBuilder::new(self.dlist_words);
        for (i, layer) in layers.iter().enumerate() {
            if layer.width_px > self.mode.width_px || layer.height_px > self.mode.height_px {
                return Err(DriverError::LengthOutOfRange);
            }
            let plane = self.planes[i].as_ref().ok_or(DriverError::DeviceFault)?;
            self.upload_plane(plane, layer)?;
            builder.push_plane(plane.bus_addr, self.mode.format, layer)?;
        }
        builder.finish()?;
        builder.write_to(&self.dlist)?;

        self.present_generation = self.present_generation.wrapping_add(1);
        self.control
            .write_u32(dlist::CONTROL_DLIST_HEAD_OFFSET, 0)
            .map_err(WindowError::as_driver_error)?;
        self.control
            .write_u32(dlist::CONTROL_GENERATION_OFFSET, self.present_generation)
            .map_err(WindowError::as_driver_error)?;
        Ok(())
    }
}
