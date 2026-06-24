//! [`RtDriverHost`] — the rt-backed [`DriverHost`] a user-space driver links.

use core::cell::Cell;
use core::ptr::NonNull;

use rustos_abi::driver::dma::{DmaHost, DmaSlab, PoolId, SlabCoherencyFn};
use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::mailbox_ipc;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, Errno, MmioMapError, MmioMapper,
    RegisterWindow,
};
use rustos_caps::CapabilitySet;

use crate::syscalls::GrantSyscalls;

/// Maximum number of device-resource grants a driver process holds.
///
/// A validation bound, not a scalable capacity: a single matched
/// hardware-tree node requests only a handful of resources (a register
/// window, an outbound bus window, a DMA constraint, an IRQ line), so a
/// table this small covers every real driver. A grant list longer than this
/// is a packaging defect and is refused fail-closed at construction.
pub const MAX_GRANTS: usize = 8;

/// One kernel-issued device-resource grant the host can map: the unforgeable
/// handle plus the [`HwResource`] it names.
///
/// A driver process receives these at spawn (the kernel mints one per
/// resource its matched node requested) and learns them
/// through the `resource_grants` syscall; [`RtDriverHost::from_grants_query`]
/// builds the host's grant table from that delivery. The single wire/owning
/// definition lives in `lib/abi` ([`rustos_abi::hwtree::GrantedResource`]) —
/// the kernel serialises it and this host decodes it, one type for both ends — and is re-exported here so a driver names it through
/// its host crate.
pub use rustos_abi::hwtree::GrantedResource;

/// A grant the host can map, plus the lazily-cached `(offset, base_va)` of
/// the sub-region last mapped from it.
struct GrantSlot {
    handle: u64,
    resource: HwResource,
    /// The `(offset, base_va)` of the most recently mapped sub-region of
    /// this grant, or `None` while none has been mapped. A driver maps a
    /// bounded `[offset, offset + len)` sub-region of its grant (not the
    /// whole window), so the cache is keyed by `offset`:
    /// a repeat request for the same sub-region reuses the cached VA, while a
    /// request at a different offset (a second BAR in the same outbound
    /// window) maps afresh (no repeated syscall for the
    /// same window). A real mapping never bases at VA `0` (it is a user
    /// address above the image bias).
    mapped: Cell<Option<(u64, u64)>>,
}

/// The user-space driver host: maps kernel-issued device-resource grants over
/// the `mmio_map` / `dma_alloc` syscalls.
///
/// Implements [`DriverHost`] (the entry surface a driver's `register`
/// consumes), [`MmioMapper`] (register-window mapping), and [`VirtioHost`]
/// (the DMA-buffer carve a bus driver allocates its device-shared structures
/// from) over one small table of grants. See the crate-level docs for the
/// design and the "not a privileged path" contract.
pub struct RtDriverHost<S: GrantSyscalls> {
    caps: CapabilitySet,
    syscalls: S,
    grants: [Option<GrantSlot>; MAX_GRANTS],
    dma_pool: PoolId,
    next_slot: Cell<usize>,
    coherency: Option<SlabCoherencyFn>,
    /// The kernel-issued [`rustos_abi::IrqHandle`] for this driver's granted
    /// interrupt line, bound lazily on the first [`VirtioHost::notify_wait`]
    /// and cached so the line is bound at most once. `0`
    /// ([`rustos_abi::IrqHandle::INVALID`]) is the unbound sentinel — a real
    /// handle is always `≥ 1` (no repeated bind syscall).
    irq_handle: Cell<u64>,
}

impl<S: GrantSyscalls> RtDriverHost<S> {
    /// Build a host over the load-time capability set `caps`, the syscall seam
    /// `syscalls`, and the kernel-issued `grants`.
    ///
    /// `coherency` is the cache-maintenance shim for a **non-coherent** DMA
    /// interconnect (e.g. the BCM2711 PCIe master, which does not snoop the
    /// CPU caches); pass `None` on a coherent interconnect (and for the QEMU
    /// `virt` stand-in), where the kernel's coherent carve needs no CPU-side
    /// maintenance. The shim is supplied by the (architecture-aware) driver
    /// process, never synthesised here, so this crate stays platform-neutral.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if `grants` holds more than
    /// [`MAX_GRANTS`] entries (a packaging defect, refused fail-closed).
    pub fn new(
        caps: CapabilitySet,
        syscalls: S,
        grants: &[GrantedResource],
        coherency: Option<SlabCoherencyFn>,
    ) -> Result<Self, DriverError> {
        if grants.len() > MAX_GRANTS {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut slots: [Option<GrantSlot>; MAX_GRANTS] = core::array::from_fn(|_| None);
        for (slot, granted) in slots.iter_mut().zip(grants.iter()) {
            *slot = Some(GrantSlot {
                handle: granted.handle,
                resource: granted.resource,
                mapped: Cell::new(None),
            });
        }
        Ok(Self::from_slots(caps, syscalls, slots, coherency))
    }

    /// Build a host by querying the kernel for the grants it minted for this
    /// driver process — the production path a `devmgr`-autoloaded driver
    /// uses at start-up (`plans/PI.md` P10 chunk 5d-2).
    ///
    /// Issues the `resource_grants` syscall through `syscalls` into a
    /// fixed-capacity buffer (sized for [`MAX_GRANTS`], so the call is
    /// allocation-free and works before the userland heap, `plans/SPAWN.md`
    /// `SP5b`), decodes the delivered [`GrantedResource`] records, and builds
    /// the host's grant table from them. The kernel minted one grant per
    /// [`HwResource`] the driver's matched node requested,
    /// so the table is exactly the resources this driver may map — no more.
    ///
    /// `coherency` is the cache-maintenance shim for a non-coherent DMA
    /// interconnect, exactly as for [`Self::new`].
    ///
    /// # Errors
    ///
    /// Fails closed without partially constructing a host:
    /// [`DriverError::LengthOutOfRange`] if the kernel minted more grants than
    /// [`MAX_GRANTS`] (a packaging defect — the delivery would not fit), and
    /// [`DriverError::Unsupported`] for any other kernel refusal or an
    /// impossible delivery (a byte count past the buffer, a partial record,
    /// or a record that fails to decode).
    pub fn from_grants_query(
        caps: CapabilitySet,
        syscalls: S,
        coherency: Option<SlabCoherencyFn>,
    ) -> Result<Self, DriverError> {
        // Read the kernel-minted grant set into a fixed buffer sized for the
        // host's `MAX_GRANTS` cap — allocation-free.
        let mut buf = [0u8; MAX_GRANTS * GrantedResource::WIRE_LEN];
        let ret = syscalls.resource_grants(&mut buf);
        if ret < 0 {
            // -errno: a `BufferTooSmall` means the kernel minted more than
            // `MAX_GRANTS` grants (a packaging defect); any other code is a
            // kernel refusal. Both fail closed.
            return Err(grants_query_error(ret));
        }
        // `ret >= 0` (checked above); a byte count the kernel wrote into a
        // buffer it was handed, so it fits `usize` on every target — but
        // convert fail-closed rather than truncating.
        let Ok(written) = usize::try_from(ret) else {
            return Err(DriverError::Unsupported);
        };
        // The kernel writes whole records into the buffer it was handed; a
        // length past the buffer or not a whole number of records is an
        // impossible delivery — refuse it rather than decode garbage
        // (validate every input).
        if written > buf.len() || written % GrantedResource::WIRE_LEN != 0 {
            return Err(DriverError::Unsupported);
        }
        let count = written / GrantedResource::WIRE_LEN;
        let mut slots: [Option<GrantSlot>; MAX_GRANTS] = core::array::from_fn(|_| None);
        for (i, slot) in slots.iter_mut().take(count).enumerate() {
            let off = i * GrantedResource::WIRE_LEN;
            let granted = GrantedResource::from_bytes(&buf[off..off + GrantedResource::WIRE_LEN])
                .map_err(|_| DriverError::Unsupported)?;
            *slot = Some(GrantSlot {
                handle: granted.handle,
                resource: granted.resource,
                mapped: Cell::new(None),
            });
        }
        Ok(Self::from_slots(caps, syscalls, slots, coherency))
    }

    /// The [`HwResource`]s the kernel granted this driver, in delivery order.
    ///
    /// A driver process derives its concrete bring-up inputs — the register
    /// BAR window and the DMA aperture bound — from the same grant set the
    /// host maps over, rather than re-querying the kernel: the host is built
    /// from the delivered grants once and this exposes them read-only so the
    /// driver's start-up reads them without a second `resource_grants`
    /// syscall. The grants are exactly the resources the
    /// matched node requested — no more.
    pub fn resources(&self) -> impl Iterator<Item = &HwResource> {
        self.grants.iter().flatten().map(|slot| &slot.resource)
    }

    /// Assemble a host from an already-built grant-slot array (the shared
    /// tail of [`Self::new`] and [`Self::from_grants_query`]).
    fn from_slots(
        caps: CapabilitySet,
        syscalls: S,
        grants: [Option<GrantSlot>; MAX_GRANTS],
        coherency: Option<SlabCoherencyFn>,
    ) -> Self {
        Self {
            caps,
            syscalls,
            grants,
            dma_pool: PoolId::fresh(),
            next_slot: Cell::new(0),
            coherency,
            irq_handle: Cell::new(0),
        }
    }

    /// Find the grant covering the mappable window `[req_base, req_base + len)`
    /// and the request's offset into that grant's mapped window.
    ///
    /// Only [`HwResourceKind::Mmio`] (CPU/identity space) and
    /// [`HwResourceKind::BusWindow`] (outbound PCIe-bus space) grants are
    /// mappable register windows. A [`BusWindow`](HwResourceKind::BusWindow)
    /// is addressed in PCIe-bus space (its [`translated_base`]), so a BAR the
    /// driver names by its bus address resolves to the same offset into the
    /// CPU window the kernel mapped — the bridge's bus→CPU translation, performed once here rather than in the
    /// architecture-neutral PCI walk.
    ///
    /// Returns the matching slot and the in-window byte `offset`, or `None`
    /// if no grant covers the whole request (fail closed).
    ///
    /// [`translated_base`]: HwResource::translated_base
    fn resolve(&self, req_base: u64, len: usize) -> Option<(&GrantSlot, u64)> {
        let req_end = req_base.checked_add(len as u64)?;
        for slot in self.grants.iter().flatten() {
            let window_start = match slot.resource.kind() {
                Some(HwResourceKind::Mmio) => slot.resource.base(),
                Some(HwResourceKind::BusWindow) => slot.resource.translated_base(),
                // A DMA constraint, IRQ line, or port range is not a mappable
                // register window (validate the kind).
                _ => continue,
            };
            let window_end = window_start.checked_add(slot.resource.length())?;
            if req_base >= window_start && req_end <= window_end {
                return Some((slot, req_base - window_start));
            }
        }
        None
    }

    /// Map the `[offset, offset + len)` sub-region of `slot`'s granted window
    /// through the `mmio_map` syscall, returning its base VA; reuse the cached
    /// base on a repeat request for the **same** sub-region offset.
    ///
    /// Mapping a bounded sub-region rather than the whole grant is what lets a
    /// driver granted a large outbound bus aperture map just the single BAR it
    /// enumerated, instead of the entire window — which would exhaust the
    /// per-task MMIO virtual window and fail closed with `OutOfMemory`. The kernel re-validates the sub-region against the
    /// grant on the far side of the trap.
    fn ensure_mapped(
        &self,
        slot: &GrantSlot,
        offset: u64,
        len: usize,
    ) -> Result<u64, MmioMapError> {
        if let Some((cached_offset, cached_va)) = slot.mapped.get() {
            if cached_offset == offset {
                return Ok(cached_va);
            }
        }
        let ret = self.syscalls.mmio_map(slot.handle, offset, len);
        if ret <= 0 {
            return Err(mmio_error(ret));
        }
        #[allow(clippy::cast_sign_loss)] // `ret > 0` checked above; it is a user VA.
        let va = ret as u64;
        slot.mapped.set(Some((offset, va)));
        Ok(va)
    }

    /// The DMA-constraint grant, if the driver was granted one.
    fn dma_grant(&self) -> Option<&GrantSlot> {
        self.grants
            .iter()
            .flatten()
            .find(|slot| slot.resource.kind() == Some(HwResourceKind::Dma))
    }

    /// The interrupt line of the driver's [`HwResourceKind::Irq`] grant, if it
    /// was granted one. [`HwResource::base`] holds the
    /// first line of an IRQ resource; an out-of-range line value is refused
    /// fail-closed (a `u32` line cannot exceed the kernel's bind ceiling once
    /// truncated — the kernel re-validates on the far side of the trap).
    fn irq_line(&self) -> Option<u32> {
        let slot = self
            .grants
            .iter()
            .flatten()
            .find(|slot| slot.resource.kind() == Some(HwResourceKind::Irq))?;
        u32::try_from(slot.resource.base()).ok()
    }
}

impl<S: GrantSyscalls> MmioMapper for RtDriverHost<S> {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        // Capability before state; the kernel re-checks.
        if !self.caps.contains(CapabilityId::MMIO_MAP) {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let (slot, offset) = self
            .resolve(phys_base, len)
            .ok_or(MmioMapError::InvalidRegion)?;
        // Map only the resolved `[offset, offset + len)` sub-region of the
        // grant — never the whole window — so a BAR inside a large outbound
        // bus aperture costs `len` bytes of mapping, not the aperture's full
        // extent. The kernel returns the base VA of that
        // sub-region directly.
        let window_va = self.ensure_mapped(slot, offset, len)?;
        let addr = usize::try_from(window_va).map_err(|_| MmioMapError::InvalidRegion)?;
        let base = NonNull::new(addr as *mut u8).ok_or(MmioMapError::InvalidRegion)?;
        // SAFETY: `ensure_mapped` obtained `window_va` from the `mmio_map`
        // syscall, which mapped exactly the `[offset, offset + len)`
        // sub-region of `slot`'s granted window (caching-disabled,
        // user-accessible, never executable) into this process's own
        // address space and kept it valid for the process's lifetime (longer
        // than the returned window). `resolve` proved `[phys_base, phys_base +
        // len)` lies wholly inside that window at `offset`, so the `len` bytes
        // from `window_va` are in-bounds, ≥ 4-byte aligned (the kernel maps
        // page-aligned and a real device offset is register-aligned), and
        // exclusively owned by this window. `phys_base` is the device-visible
        // base the window records (the kernel validated the
        // grant and the sub-region bounds).
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

impl<S: GrantSyscalls> DmaHost for RtDriverHost<S> {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        // Capability before state; the kernel re-checks.
        if !self.caps.contains(CapabilityId::MEM_DMA) {
            return Err(DriverError::PermissionDenied);
        }
        if size == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let grant = self.dma_grant().ok_or(DriverError::Unsupported)?;
        let mut device: u64 = 0;
        let ret = self.syscalls.dma_alloc(grant.handle, size, &mut device);
        if ret <= 0 {
            return Err(dma_error(ret));
        }
        #[allow(clippy::cast_sign_loss)] // `ret > 0` checked above; it is a user VA.
        let cpu_va = ret as u64;
        let addr = usize::try_from(cpu_va).map_err(|_| DriverError::OutOfRange)?;
        let ptr = NonNull::new(addr as *mut u8).ok_or(DriverError::DeviceFault)?;
        let slot = self.next_slot.get();
        self.next_slot.set(slot.wrapping_add(1));
        // SAFETY: `dma_alloc` carved exactly `size` bytes of zeroed,
        // physically-contiguous, coherent, `RW` (non-executable),
        // guard-bracketed memory mapped into this process's own address space,
        // and kept it valid for the process's lifetime (longer than the
        // returned slab — there is no userland free, the kernel reclaims it on
        // exit via `LiveSpace::Drop`). `ptr` is its non-null,
        // page-aligned CPU base and `device` its device-visible base; the
        // region is exclusively this slab's (a fresh carve per call), so no
        // other live reference aliases it. The slab's drop is a no-op
        // (`from_leaked`): the kernel owns reclamation.
        let slab = unsafe { DmaSlab::from_leaked(device, ptr, size, self.dma_pool, slot) };
        Ok(match self.coherency {
            Some(coherency) => slab.with_coherency(coherency),
            None => slab,
        })
    }
}

impl<S: GrantSyscalls> VirtioHost for RtDriverHost<S> {
    fn notify_wait(&self, _queue_index: u16) {
        // Park the driver on its granted device interrupt line until the
        // device signals queue activity (an
        // interrupt-driven driver parks, never busy-spins). A virtio device
        // raises one MSI/MMIO line (not per-queue), so `queue_index` is not
        // part of the wait key: the driver re-scans every used ring on wake.
        //
        // The line is bound lazily on the first call and cached, so the bind
        // syscall runs at most once. The kernel re-arms
        // the line across each park on the driver's behalf — the driver holds
        // no controller access — so this just `irq_wait`s the bound
        // handle. A driver granted no IRQ line (or lacking `CAP_IRQ_BIND`)
        // returns without parking; its caller then re-polls and yields
        // (fail safe, never a wedged wait).
        let mut handle = self.irq_handle.get();
        if handle == 0 {
            // Capability before the trap; the kernel
            // re-checks `CAP_IRQ_BIND` regardless.
            if !self.caps.contains(CapabilityId::IRQ_BIND) {
                return;
            }
            let Some(line) = self.irq_line() else {
                return;
            };
            let ret = self.syscalls.irq_bind(line);
            if ret <= 0 {
                return;
            }
            #[allow(clippy::cast_sign_loss)]
            // `ret > 0` checked above; it is a kernel-minted handle.
            let bound = ret as u64;
            self.irq_handle.set(bound);
            handle = bound;
        }
        // Unbounded wait: the loop terminates on a fire, a binding release
        // (a spurious wake the caller tolerates by re-scanning), or never —
        // the device is the only thing that completes a virtio request. The
        // terminal outcome is intentionally discarded; the trait returns `()`
        // and the caller re-checks its rings on return.
        let _ = self.syscalls.irq_wait(handle, u64::MAX);
    }
}

impl<S: GrantSyscalls> MailboxChannel for RtDriverHost<S> {
    /// Marshal one [`MAILBOX_PROPERTY_WORDS`]-word property exchange over the
    /// kernel's synchronous call surface to the user-space mailbox service
    /// ([`mailbox_ipc::MAILBOX_ENDPOINT`]): encode the request, `ipc_call`,
    /// and decode the firmware's response back into `message` in place.
    ///
    /// The host owns no doorbell registers and no DMA buffer here — the
    /// `vcmailbox` service does — so this is purely the client side of the
    /// IPC. The kernel gates the call by the
    /// endpoint's required send capability (`CAP_MAILBOX`) and copies both
    /// buffers through the validated boundary; this host adds no authority. Every failure path fails closed to a
    /// [`DriverError`], never a panic.
    fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        let mut request = [0u8; mailbox_ipc::REQUEST_LEN];
        mailbox_ipc::encode_request(&mut request, message)
            .map_err(|_| DriverError::BufferTooSmall)?;
        let mut reply = [0u8; mailbox_ipc::REPLY_LEN];
        let ret = self
            .syscalls
            .ipc_call(mailbox_ipc::MAILBOX_ENDPOINT, &request, &mut reply);
        if ret < 0 {
            return Err(decode_errno(ret).map_or(DriverError::DeviceFault, ipc_driver_error));
        }
        // `ret >= 0` checked above; the result is then clamped to `reply.len()`,
        // so any truncation on a 32-bit target cannot drive an out-of-bounds
        // slice (defence in depth).
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let n = (ret as usize).min(reply.len());
        mailbox_ipc::decode_reply(&reply[..n], message).map_err(ipc_driver_error)
    }
}

impl<S: GrantSyscalls> DriverHost for RtDriverHost<S> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        Some(self)
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self)
    }

    fn dma_host(&self) -> Option<&dyn DmaHost> {
        Some(self)
    }

    fn mailbox(&self) -> Option<&dyn MailboxChannel> {
        // Every rt-backed host can reach the firmware-mailbox service through
        // the kernel's call surface; whether a given driver *may* is enforced
        // kernel-side by the endpoint's `CAP_MAILBOX` send gate, not here. A driver without the capability simply has its
        // `exchange` fail closed.
        Some(self)
    }

    fn emit_node(&self, node: rustos_abi::HwNode) -> Result<(), DriverError> {
        // Publish the enumerated child through the `hw_emit_node` syscall so
        // the device manager autoloads its driver in turn. The host adds no authority: the kernel gates the call by
        // `CAP_HW_EMIT` and admits the node only when every resource it
        // requests is covered by one of this driver's own grants (no ambient authority). A refusal fails closed.
        let ret = self.syscalls.hw_emit_node(&node);
        if ret < 0 {
            return Err(decode_errno(ret).map_or(DriverError::DeviceFault, emit_node_error));
        }
        Ok(())
    }
}

/// Map a non-positive `mmio_map` result to a [`MmioMapError`].
///
/// `ret` is `≤ 0`: a negative value is `-errno`, and `0` is an impossible base
/// VA the kernel never returns for a real mapping (treated as a platform
/// failure). The capability was already checked locally, so a kernel
/// `PermissionDenied` here still maps to [`MmioMapError::CapabilityMissing`]
/// (the authoritative kernel verdict); a bad/forged grant or out-of-range
/// window maps to [`MmioMapError::InvalidRegion`]; anything else is an
/// [`MmioMapError::Unsupported`] platform refusal.
fn mmio_error(ret: i64) -> MmioMapError {
    match decode_errno(ret) {
        Some(Errno::PermissionDenied) => MmioMapError::CapabilityMissing,
        Some(Errno::OutOfRange | Errno::LengthOutOfRange | Errno::NotFound | Errno::BadAddress) => {
            MmioMapError::InvalidRegion
        }
        _ => MmioMapError::Unsupported,
    }
}

/// Map a non-positive `dma_alloc` result to a [`DriverError`].
///
/// `ret` is `≤ 0`. A kernel `PermissionDenied` maps to
/// [`DriverError::PermissionDenied`]; an exhausted pool / over-limit / oversize
/// carve maps to [`DriverError::LengthOutOfRange`] (the documented
/// [`DmaHost::alloc_dma_zeroed`] exhaustion error); anything else (an inert
/// facility, an unknown code, a `0` base) is [`DriverError::Unsupported`].
fn dma_error(ret: i64) -> DriverError {
    match decode_errno(ret) {
        Some(Errno::PermissionDenied) => DriverError::PermissionDenied,
        Some(Errno::LengthOutOfRange | Errno::OutOfMemory | Errno::OutOfRange) => {
            DriverError::LengthOutOfRange
        }
        _ => DriverError::Unsupported,
    }
}

/// Map a negative `resource_grants` result to a [`DriverError`].
///
/// `ret` is `< 0` (`-errno`). A `BufferTooSmall` means the kernel minted more
/// grants than the host's [`MAX_GRANTS`] cap can hold — a packaging defect
/// surfaced as [`DriverError::LengthOutOfRange`]; anything else is a kernel
/// refusal surfaced as [`DriverError::Unsupported`].
fn grants_query_error(ret: i64) -> DriverError {
    match decode_errno(ret) {
        Some(Errno::BufferTooSmall) => DriverError::LengthOutOfRange,
        _ => DriverError::Unsupported,
    }
}

/// Map an [`Errno`] surfaced by the mailbox `ipc_call` (a transport `-errno`
/// or the service's status-framed reply) to a [`DriverError`] the
/// [`MailboxChannel`] reports.
///
/// `PermissionDenied` (the caller lacks `CAP_MAILBOX`) and `NotFound` (no
/// service is serving the endpoint) keep their identity; everything else —
/// including the service's own `NotImplemented` image of a device fault /
/// timeout (`DriverError::as_errno`) — folds to [`DriverError::DeviceFault`]
/// so the exchange fails closed.
fn ipc_driver_error(errno: Errno) -> DriverError {
    match errno {
        Errno::PermissionDenied => DriverError::PermissionDenied,
        Errno::NotFound => DriverError::NotFound,
        _ => DriverError::DeviceFault,
    }
}

/// Map an [`Errno`] surfaced by a refused `hw_emit_node` to a [`DriverError`].
///
/// `PermissionDenied` (the driver lacks `CAP_HW_EMIT`, or the node requests a
/// resource outside its grants) keeps its identity so the bus driver sees the
/// authority refusal; everything else — a malformed node, an unknown parent, a
/// build with no store wired — folds to [`DriverError::DeviceFault`] so the
/// publish fails closed.
fn emit_node_error(errno: Errno) -> DriverError {
    match errno {
        Errno::PermissionDenied => DriverError::PermissionDenied,
        _ => DriverError::DeviceFault,
    }
}

/// Recover the [`Errno`] a negative `abi-v1` result register encodes (`-errno`),
/// or `None` for a non-negative `ret` or an unknown discriminant.
fn decode_errno(ret: i64) -> Option<Errno> {
    let code = ret.checked_neg()?;
    let code = i32::try_from(code).ok()?;
    Errno::from_i32(code)
}
