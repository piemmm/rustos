//! Browser-headless `display`-vertical for the wasm32 Arch HAL
//! (`plans/WIRING.md` — the wasm32 `display`-row parity gap).
//!
//! Built for `wasm32-unknown-unknown` this `cdylib` is the wasm32
//! analogue of the bare-metal `framebuffer_display_qemu_{riscv64,aarch64}`
//! verticals. Where those synthesise a `ramfb` scan-out surface in guest
//! RAM and read it back through an independent MMIO mapping, this one
//! drives the same signed-driver lifecycle against a static surface in
//! WASM linear memory and presents the result to a **real HTML canvas**
//! (logical CPU 0, the main browser thread):
//!
//! * **Boots.** The port's `rustos_arch_wasm32_main` export forwards to
//!   the `kernel_main` here, which prints `BOOT_OK`.
//! * **Signed `.rxe` load gate.** The build-time signed framebuffer
//!   display `.rxe` is loaded through `rustos_drvhost::Host` (the
//!   gate) and driven through `load -> use -> unload -> reload`.
//! * **Capability-gated surface map.** "use" maps the surface through a
//!   capability-checked `WasmMmioMapper` (the wasm32 analogue of the
//!   kernel MMIO mapper — there is no MMU, so a window is a checked view
//!   of the in-memory surface) and `present`s a frame.
//! * **Two independent read-backs.** Each presented frame is confirmed
//!   twice: once through a *second, independently-mapped* window over the
//!   surface (the bytes reached linear memory) and once through the host
//!   `rustos_host_present_framebuffer` import, which paints the surface
//!   onto a canvas and returns the count of pixels that survived the
//!   canvas round-trip — proving the pixels reach a genuine display
//!   surface. On success it prints `DISPLAY_OK`.
//!
//! The browser harness (`web/harness.mjs`, launched by `cargo xtask test
//! --wasm`) scrapes those console markers and reports PASS once it has
//! seen `BOOT_OK` and `DISPLAY_OK`; any panic traps the instance and
//! fails the run loudly.
//!
//! On a host build (`itest_wasm32` off) this compiles to an inert empty
//! `cdylib`, exactly as the bare-metal verticals are inert host stubs, so
//! `cargo build --workspace` stays green without the wasm toolchain.
#![cfg_attr(itest_wasm32, no_std)]
#![deny(missing_docs)]

#[cfg(itest_wasm32)]
extern crate alloc;

#[cfg(itest_wasm32)]
mod fixture {
    //! Build-time generated signed `.rxe` fixture + trust anchor.
    include!(concat!(env!("OUT_DIR"), "/fb_fixture.rs"));
}

#[cfg(itest_wasm32)]
mod kernel {
    use core::panic::PanicInfo;
    use core::ptr::NonNull;

    use alloc::vec::Vec;

    use rustos_abi::driver::display::{Display, DisplayFormat};
    use rustos_abi::driver::{MmioMapError, MmioMapper, RegisterWindow};
    use rustos_abi::{CapabilityId, DriverError, DriverHost, DriverKind, Errno};
    use rustos_arch_wasm32::bindings::{host_has_display, host_present_framebuffer};
    use rustos_arch_wasm32::console::write_line;
    use rustos_arch_wasm32::handle_panic_via_console;
    use rustos_caps::CapabilitySet;
    use rustos_crypto::Ed25519PublicKey;
    use rustos_display::{Framebuffer, FramebufferConfig};
    use rustos_drvhost::{
        DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
    };
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};

    use crate::fixture::{FB_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

    // --- Framebuffer geometry ----------------------------------------

    /// Surface width in pixels.
    const WIDTH: u32 = 64;
    /// Surface height in pixels.
    const HEIGHT: u32 = 64;
    /// Bytes per pixel (32-bit colour).
    const BPP: u32 = 4;
    /// Scanline stride in bytes (no padding).
    const STRIDE: u32 = WIDTH * BPP;
    /// Total surface size in bytes.
    const FB_BYTES: usize = (STRIDE * HEIGHT) as usize;
    /// Total pixel count, which a lossless canvas round-trip must match.
    const PIXELS: u32 = WIDTH * HEIGHT;

    // --- Per-instance boot heap --------------------------------------

    /// Per-instance boot heap. The display vertical runs only on the
    /// boot context (logical CPU 0), which owns this linear memory.
    ///
    /// `static mut` because the bump allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the `HEAP` static outlives the instance and the allocator
    /// is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Static scan-out surface -------------------------------------

    /// Page-aligned wrapper so the surface meets the window's ≥ 4-byte
    /// word-access alignment contract and starts on a frame boundary.
    #[repr(C, align(4096))]
    struct Surface([u8; FB_BYTES]);

    /// The display surface, in this instance's WASM linear memory. It
    /// must outlive every window mapped over it, so it is a `static`
    /// rather than a stack/heap buffer.
    static mut FRAMEBUFFER: Surface = Surface([0u8; FB_BYTES]);

    /// Base pointer of [`FRAMEBUFFER`].
    fn framebuffer_ptr() -> *mut u8 {
        core::ptr::addr_of_mut!(FRAMEBUFFER) as *mut u8
    }

    /// "Physical" base address of [`FRAMEBUFFER`] — in the WASM model
    /// the linear-memory offset doubles as the device-visible address.
    fn framebuffer_phys() -> u64 {
        framebuffer_ptr() as u64
    }

    /// Borrow the whole surface as a byte slice for the canvas present.
    fn framebuffer_bytes() -> &'static [u8] {
        // SAFETY: `FRAMEBUFFER` is a live `static` of exactly `FB_BYTES`
        // bytes; the boot context is single-threaded at the point this
        // is read (after `present` returned), so there is no concurrent
        // writer aliasing the shared reference.
        unsafe { core::slice::from_raw_parts(framebuffer_ptr(), FB_BYTES) }
    }

    // --- Failure path ------------------------------------------------

    /// Emit `msg` and trap the instance so a regression fails loudly
    /// instead of silently reporting success.
    fn fail(msg: &str) -> ! {
        write_line(msg);
        panic!("framebuffer-wasm32 vertical failed");
    }

    /// Forward this module's panics to the shared console bridge, which
    /// emits one record and traps the instance.
    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_console(info)
    }

    // --- Capability-checked MMIO mapper ------------------------------

    /// A capability-checked view of the in-memory display surface — the
    /// wasm32 analogue of the kernel MMIO mapper. WebAssembly has no MMU,
    /// so a "register window" is a bounds- and capability-checked view of
    /// the one surface this instance owns: a request is honoured only
    /// when the caller holds [`CapabilityId::MMIO_MAP`] and the requested
    /// `[phys_base, phys_base + len)` lies wholly inside the surface.
    struct WasmMmioMapper {
        granted: CapabilitySet,
        base: u64,
        len: usize,
    }

    impl WasmMmioMapper {
        fn new(granted: CapabilitySet) -> Self {
            Self {
                granted,
                base: framebuffer_phys(),
                len: FB_BYTES,
            }
        }
    }

    impl MmioMapper for WasmMmioMapper {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
            if !self.granted.contains(CapabilityId::MMIO_MAP) {
                return Err(MmioMapError::CapabilityMissing);
            }
            if len == 0 {
                return Err(MmioMapError::InvalidRegion);
            }
            let end = phys_base
                .checked_add(len as u64)
                .ok_or(MmioMapError::InvalidRegion)?;
            let surface_end = self.base + self.len as u64;
            if phys_base < self.base || end > surface_end {
                return Err(MmioMapError::Unsupported);
            }
            let offset = (phys_base - self.base) as usize;
            // SAFETY: `offset + len <= FB_BYTES` (checked above), so the
            // pointer addresses bytes wholly inside the live `FRAMEBUFFER`
            // static; `base` is 4096-aligned and the surface outlives the
            // window. This instance is single-threaded, so the window has
            // unique access to its byte range (`from_mapping`'s contract).
            let ptr = unsafe { NonNull::new_unchecked(framebuffer_ptr().add(offset)) };
            Ok(unsafe { RegisterWindow::from_mapping(phys_base, ptr, len) })
        }
    }

    // --- Host plumbing -----------------------------------------------

    /// Image source returning the baked-in signed `.rxe` regardless of
    /// path.
    struct BakedSource;

    impl ImageSource for BakedSource {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(FB_IMAGE);
            Ok(())
        }
    }

    /// Per-load `DriverHandle` marker the spawner reports. The bytes
    /// spell `"FBUF"`, mirroring the marker the framebuffer driver's
    /// `register` entry point used before it became a spawned `Run`
    /// process.
    const FB_HANDLE_MARKER: u64 = 0x4642_5546_0000_0001;

    /// Spawner clearing every verified manifest through the load-time
    /// capability gate. The framebuffer driver is a spawned `Run`
    /// process in production (its engine lives in `lib/display`), so
    /// the vertical's in-process spawner carries only the gate a
    /// register entry point would have enforced: no `CAP_DRV_LOAD`, no
    /// load.
    struct ResolveFramebuffer;

    impl DriverSpawner for ResolveFramebuffer {
        fn spawn_and_register(
            &self,
            ctx: &SpawnContext<'_>,
        ) -> Result<rustos_abi::DriverHandle, SpawnRegisterError> {
            if !ctx.host.has_capability(CapabilityId::DRV_LOAD) {
                return Err(SpawnRegisterError::Register(DriverError::PermissionDenied));
            }
            rustos_abi::DriverHandle::from_raw(FB_HANDLE_MARKER)
                .map_err(SpawnRegisterError::Register)
        }
    }

    /// Driver-host view used for `Framebuffer::open`: grants
    /// `CAP_MMIO_MAP` and exposes the [`WasmMmioMapper`]. Distinct from
    /// the [`Host`]-installed load view, mirroring how the bus-driver
    /// verticals separate the load gate from the map gate.
    struct FramebufferHost<'a> {
        granted: CapabilitySet,
        mapper: &'a dyn MmioMapper,
    }

    impl DriverHost for FramebufferHost<'_> {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            self.granted.contains(cap)
        }

        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }

        fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
            Some(self.mapper)
        }
    }

    // --- Frame patterns ----------------------------------------------

    /// Deterministic test frame keyed by `salt` so the two phases write
    /// distinguishable surfaces (a stale read cannot pass the second
    /// phase). Every alpha byte is `0xFF` (fully opaque) so the surface
    /// survives the canvas premultiplied-alpha round-trip unchanged.
    fn make_frame(salt: u8) -> Vec<u8> {
        (0..FB_BYTES)
            .map(|i| {
                if i % 4 == 3 {
                    0xFF
                } else {
                    (u8::try_from(i & 0xFF).unwrap_or(0)).wrapping_mul(7) ^ salt
                }
            })
            .collect()
    }

    /// Map a fresh window over the surface and confirm its first
    /// `FB_BYTES` bytes equal `expected`. Never returns on mismatch.
    fn verify_surface(mapper: &dyn MmioMapper, phys_base: u64, expected: &[u8]) {
        let Ok(window) = mapper.map_window(phys_base, FB_BYTES) else {
            fail("HARNESS_ERROR verify: map_window failed");
        };
        let mut off = 0;
        while off < FB_BYTES {
            let Ok(got) = window.read_u32(off) else {
                fail("HARNESS_ERROR verify: window read out of bounds");
            };
            let want = u32::from_le_bytes([
                expected[off],
                expected[off + 1],
                expected[off + 2],
                expected[off + 3],
            ]);
            if got != want {
                fail("HARNESS_ERROR verify: surface pixel mismatch");
            }
            off += 4;
        }
    }

    /// Confirm the presented surface reaches a genuine display by
    /// painting it onto the host canvas and checking every pixel
    /// survived the round-trip. Never returns on mismatch.
    fn verify_canvas() {
        let matched = host_present_framebuffer(framebuffer_bytes(), WIDTH, HEIGHT, STRIDE);
        if matched != PIXELS {
            fail("HARNESS_ERROR canvas round-trip pixel mismatch");
        }
    }

    /// Open the framebuffer through `fb_host`, present `frame`, drop the
    /// driver (the quiesce step), then confirm `frame` landed in the
    /// surface through an independent window *and* survived the canvas
    /// round-trip.
    fn present_and_verify(
        fb_host: &FramebufferHost<'_>,
        mapper: &dyn MmioMapper,
        config: FramebufferConfig,
        frame: &[u8],
        open_msg: &'static str,
        present_msg: &'static str,
    ) {
        {
            let Ok(mut fb) = Framebuffer::open(fb_host, config) else {
                fail(open_msg);
            };
            if fb.present(frame).is_err() {
                fail(present_msg);
            }
            // `fb` drops here, releasing its window handle (quiesce).
        }
        verify_surface(mapper, config.phys_base, frame);
        verify_canvas();
    }

    /// Build the capability-checked mapper, load the signed `.rxe`
    /// through the [`Host`], and drive `load -> use -> unload -> reload`
    /// against the surface, verifying each presented frame reaches both
    /// linear memory and the canvas.
    fn drive_lifecycle() {
        if !host_has_display() {
            fail("HARNESS_ERROR host exposes no display surface");
        }

        let config = FramebufferConfig {
            phys_base: framebuffer_phys(),
            width_px: WIDTH,
            height_px: HEIGHT,
            stride_bytes: STRIDE,
            format: DisplayFormat::Rgba8888,
        };

        // Capability-checked mapper holding CAP_MMIO_MAP.
        let mut map_grants = CapabilitySet::empty();
        map_grants.insert(CapabilityId::MMIO_MAP);
        let mapper = WasmMmioMapper::new(map_grants);

        // Driver-host view for `Framebuffer::open` (CAP_MMIO_MAP granted).
        let mut open_grants = CapabilitySet::empty();
        open_grants.insert(CapabilityId::MMIO_MAP);
        let fb_host = FramebufferHost {
            granted: open_grants,
            mapper: &mapper,
        };

        // Load the signed `.rxe` through the driver host (the gate).
        let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
            fail("HARNESS_ERROR trust anchor decode");
        };
        let trusted = [pubkey];
        let mut load_caps = CapabilitySet::empty();
        load_caps.insert(CapabilityId::DRV_LOAD);
        let source = BakedSource;
        let spawner = ResolveFramebuffer;
        let mut host = Host::new(HostConfig {
            trusted_signers: &trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            spawner: &spawner,
            sink: &rustos_arch_wasm32::CONSOLE_SINK,
            virtio_host_factory: None,
            mmio_mapper: None,
        });
        let Ok(h1) = host.load("/System/Drivers/framebuffer.rxe", &load_caps) else {
            fail("HARNESS_ERROR signed .rxe load");
        };
        if host.loaded_count() != 1 || host.snapshot().first().map(|s| s.handle) != Some(h1) {
            fail("HARNESS_ERROR loaded state after load");
        }

        // use: present a frame, verify in memory and on the canvas.
        present_and_verify(
            &fb_host,
            &mapper,
            config,
            &make_frame(0x00),
            "HARNESS_ERROR Framebuffer::open (first)",
            "HARNESS_ERROR present (first)",
        );

        // unload -> reload through the host (a fresh handle each time).
        let Ok(h2) = host.reload(h1, &load_caps) else {
            fail("HARNESS_ERROR signed .rxe reload");
        };
        if h2 == h1 || host.loaded_count() != 1 {
            fail("HARNESS_ERROR loaded state after reload");
        }

        // use again after reload: a distinct frame, verified both ways.
        present_and_verify(
            &fb_host,
            &mapper,
            config,
            &make_frame(0xA5),
            "HARNESS_ERROR Framebuffer::open (reloaded)",
            "HARNESS_ERROR present (reloaded)",
        );

        // unload: tear the driver down cleanly.
        if host.unload(h2).is_err() || host.loaded_count() != 0 {
            fail("HARNESS_ERROR driver unload");
        }

        write_line("DISPLAY_OK");
    }

    /// Boot body the port's `rustos_arch_wasm32_main` export forwards to
    /// once the host has instantiated the module. The display vertical
    /// runs entirely on the boot context and then returns to the host
    /// event loop.
    #[no_mangle]
    pub extern "C" fn kernel_main() {
        write_line("BOOT_OK");
        drive_lifecycle();
    }
}
