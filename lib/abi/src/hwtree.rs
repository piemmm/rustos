//! The architecture-neutral hardware tree.
//!
//! RustOS detects the hardware actually present at boot and autoloads the
//! matching drivers; it does not ship a hand-maintained static device list. The *single* inventory contract is the **hardware
//! tree** defined here: each architecture port normalises its platform's
//! native source (ACPI on x86_64, a flattened device tree on aarch64 /
//! riscv64, a host-capability query on wasm32) into a flat list of
//! [`HwNode`]s linked by parent id, and the user-space device manager
//! (`userland/system/devmgr`) matches each node's [`HwMatchKey`]s against
//! the bind table every driver declares in its signed manifest.
//!
//! # ABI discipline
//!
//! The hardware tree is held to the same discipline as the syscall table and the System Information API: it is
//! versioned ([`HWTREE_VERSION_V1`]), every record has a fixed wire layout
//! pinned by a `WIRE_LEN` constant and a frozen-layout host test, and the
//! C view is generated from this source of truth (`cargo xtask c-header`).
//! Extend the tree with a new version; never mutate a shipped one.
//!
//! # No ambient authority
//!
//! A node's resources (MMIO windows, IRQ lines, port ranges, DMA needs)
//! are expressed as capability-grant **requests** ([`HwResource`]), never
//! as raw ambient handles: a matched driver receives only
//! the resource capabilities its node requested, and no more. The
//! capability a resource needs is named explicitly as a [`CapabilityId`].
//!
//! The types are `#[repr(C)]`, `no_std`, and allocation-free: a node is a
//! fixed-size record built on the boot stack, encoded little-endian through
//! the shared `le` helpers, and decoded with every field bounds-checked
//! against `WIRE_LEN` (validate every input, fail
//! closed).

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::{CapabilityId, Errno};

/// Hardware-tree ABI version tag.
///
/// Carried in every serialised tree so a consumer can refuse a tree
/// produced for a future revision rather than misinterpreting it. Frozen
/// for `abi-v1`; new behaviour bumps the version.
pub const HWTREE_VERSION_V1: u16 = 1;

/// Sentinel parent id marking a node with no parent (a tree root).
///
/// A real node id is a small dense index; `u32::MAX` can never collide
/// with one, so it is the unambiguous "no parent" marker.
pub const HW_NODE_ROOT: u32 = u32::MAX;

/// Id of the single synthetic root node every discovered hardware tree
/// begins with.
///
/// The root node names [`HW_NODE_ROOT`] as its *parent* (so
/// [`HwNode::is_root`] holds for it alone); every real device node names
/// this id — or a deeper bus node's id — as its parent. Defining it once
/// here keeps every architecture port's root emission and every
/// bootstrap-floor probe that attaches a top-level device to the root in
/// agreement: a device parented to [`HW_NODE_ROOT`]
/// instead of this id would be mistaken for the root and skipped by the
/// autoload walk.
pub const HW_NODE_ROOT_ID: u32 = 0;

/// Maximum bytes of a device-tree / MMIO `compatible` string a match key
/// carries inline. Longer strings are rejected, never truncated.
pub const HW_COMPATIBLE_MAX: usize = 64;

/// Maximum number of [`HwMatchKey`]s a single node carries.
pub const HW_NODE_MAX_MATCH_KEYS: usize = 4;

/// Maximum number of [`HwResource`]s a single node carries.
pub const HW_NODE_MAX_RESOURCES: usize = 8;

/// Device class of a hardware-tree node.
///
/// A closed set matching the driver folder classes plus the structural classes the discovery code needs to model a
/// platform (`Root`, `Bus`, `Cpu`, `Memory`, `Timer`,
/// `InterruptController`). Adding a class is an ABI change: append a new
/// discriminant, never renumber an existing one.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub enum HwDeviceClass {
    /// The synthetic root of the tree.
    #[default]
    Root = 0,
    /// An I/O bus (PCI, USB, virtio-mmio, platform MMIO).
    Bus = 1,
    /// A logical CPU / hardware thread.
    Cpu = 2,
    /// A region of system memory.
    Memory = 3,
    /// A timer / clock source.
    Timer = 4,
    /// An interrupt controller (APIC/IO-APIC, GIC, PLIC).
    InterruptController = 5,
    /// A display / GPU device.
    Display = 6,
    /// An input device (keyboard, mouse, …).
    Input = 7,
    /// A network device.
    Network = 8,
    /// A block / storage device.
    Storage = 9,
    /// A serial / console UART.
    Serial = 10,
    /// A device whose class is not modelled by `abi-v1`.
    Other = 65535,
}

impl HwDeviceClass {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Inverse of [`Self::as_u16`]; `None` for an unknown discriminant.
    #[must_use]
    pub const fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Root),
            1 => Some(Self::Bus),
            2 => Some(Self::Cpu),
            3 => Some(Self::Memory),
            4 => Some(Self::Timer),
            5 => Some(Self::InterruptController),
            6 => Some(Self::Display),
            7 => Some(Self::Input),
            8 => Some(Self::Network),
            9 => Some(Self::Storage),
            10 => Some(Self::Serial),
            65535 => Some(Self::Other),
            _ => None,
        }
    }
}

/// The kind of identifier a [`HwMatchKey`] carries.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum HwMatchKind {
    /// A device-tree / MMIO `compatible` string (FDT, platform MMIO).
    Compatible = 0,
    /// A PCI `vendor:device:class` triple.
    Pci = 1,
    /// A USB `vid:pid:class` triple.
    Usb = 2,
    /// A virtio device id.
    Virtio = 3,
}

impl HwMatchKind {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Inverse of [`Self::as_u16`]; `None` for an unknown discriminant.
    #[must_use]
    pub const fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Compatible),
            1 => Some(Self::Pci),
            2 => Some(Self::Usb),
            3 => Some(Self::Virtio),
            _ => None,
        }
    }
}

/// One match key on a hardware-tree node.
///
/// The device manager compares these against the bind table each driver
/// declares in its signed manifest. A key is either a
/// `compatible` string (FDT / MMIO) or a numeric bus identifier (PCI, USB,
/// virtio); the [`HwMatchKind`] discriminant selects which fields are
/// meaningful. Unused numeric fields are zero.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HwMatchKey {
    kind: u16,
    compatible_len: u8,
    vendor: u16,
    product: u16,
    class: u32,
    compatible: [u8; HW_COMPATIBLE_MAX],
}

impl HwMatchKey {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 12 + HW_COMPATIBLE_MAX;

    /// A `compatible`-string match key (device tree or platform MMIO).
    ///
    /// `const`, so a driver can declare its bind table as a `const`: an over-long literal is then a *compile-time*
    /// error in const context, never a runtime panic.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `compatible` exceeds
    /// [`HW_COMPATIBLE_MAX`]; the string is never truncated.
    pub const fn compatible(compatible: &[u8]) -> Result<Self, Errno> {
        if compatible.len() > HW_COMPATIBLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; HW_COMPATIBLE_MAX];
        // `copy_from_slice` / `u8::try_from` are not `const`; copy
        // byte-wise and count into a `u8`. The loop runs at most
        // `HW_COMPATIBLE_MAX` (64) times (bounded above), so `compatible_len`
        // cannot overflow and no width-narrowing cast is needed.
        let mut compatible_len: u8 = 0;
        let mut i = 0;
        while i < compatible.len() {
            buf[i] = compatible[i];
            i += 1;
            compatible_len += 1;
        }
        Ok(Self {
            kind: HwMatchKind::Compatible.as_u16(),
            compatible_len,
            vendor: 0,
            product: 0,
            class: 0,
            compatible: buf,
        })
    }

    /// A PCI `vendor:device:class` match key.
    ///
    /// `class` is the 24-bit PCI class code
    /// `(base_class << 16) | (sub_class << 8) | prog_if` (e.g. an xHCI
    /// USB host is `0x0C_03_30`). A zero `vendor` and/or `device` in a
    /// *bind-table* key is a wildcard — see [`HwMatchKey::matches`].
    #[must_use]
    pub const fn pci(vendor: u16, device: u16, class: u32) -> Self {
        Self::numeric(HwMatchKind::Pci, vendor, device, class)
    }

    /// A USB `vid:pid:class` match key.
    ///
    /// `class` is the 24-bit USB code
    /// `(class << 16) | (sub_class << 8) | protocol` of the matched
    /// (boot) interface (e.g. an HID boot keyboard is `0x03_01_01`, a
    /// boot mouse `0x03_01_02`). A zero `vendor` and/or `product` in a
    /// *bind-table* key is a wildcard — see [`HwMatchKey::matches`].
    #[must_use]
    pub const fn usb(vendor: u16, product: u16, class: u32) -> Self {
        Self::numeric(HwMatchKind::Usb, vendor, product, class)
    }

    /// A virtio device-id match key.
    #[must_use]
    pub const fn virtio(device_id: u32) -> Self {
        Self::numeric(HwMatchKind::Virtio, 0, 0, device_id)
    }

    const fn numeric(kind: HwMatchKind, vendor: u16, product: u16, class: u32) -> Self {
        Self {
            kind: kind.as_u16(),
            compatible_len: 0,
            vendor,
            product,
            class,
            compatible: [0u8; HW_COMPATIBLE_MAX],
        }
    }

    /// Does `self`, read as a driver **bind-table** key, match `device`,
    /// a concrete key emitted on a discovered hardware-tree node?
    ///
    /// Equal kinds are required first. Then:
    ///
    /// * [`Compatible`](HwMatchKind::Compatible): the `compatible` bytes
    ///   must be byte-for-byte equal.
    /// * [`Pci`](HwMatchKind::Pci) / [`Usb`](HwMatchKind::Usb): the
    ///   `class` codes must be equal, and each of `vendor` / `product`
    ///   must either be `0` in the bind key (a **wildcard**, so a generic
    ///   class driver — an xHCI host, an HID boot device — binds without
    ///   hard-coding a vendor/device id) or equal the device's value.
    /// * [`Virtio`](HwMatchKind::Virtio): the device ids (`class`) must
    ///   be equal.
    ///
    /// Widening is only ever requested by the *bind* key (which comes
    /// from a signed manifest); a discovered node can
    /// never force a broader match. An unrecognised kind matches nothing
    /// (fail closed).
    #[must_use]
    pub fn matches(&self, device: &HwMatchKey) -> bool {
        let Some(kind) = self.kind() else {
            return false;
        };
        if Some(kind) != device.kind() {
            return false;
        }
        match kind {
            HwMatchKind::Compatible => self.compatible_bytes() == device.compatible_bytes(),
            HwMatchKind::Virtio => self.class == device.class,
            HwMatchKind::Pci | HwMatchKind::Usb => {
                self.class == device.class
                    && (self.vendor == 0 || self.vendor == device.vendor)
                    && (self.product == 0 || self.product == device.product)
            }
        }
    }

    /// The kind of identifier this key carries, or [`None`] if the wire
    /// discriminant is unknown.
    #[must_use]
    pub fn kind(&self) -> Option<HwMatchKind> {
        HwMatchKind::from_u16(self.kind)
    }

    /// The `compatible` string bytes ([`HwMatchKind::Compatible`] only;
    /// empty for a numeric key).
    #[must_use]
    pub fn compatible_bytes(&self) -> &[u8] {
        &self.compatible[..usize::from(self.compatible_len)]
    }

    /// The PCI/USB vendor id (`0` for a non-vendor key).
    #[must_use]
    pub const fn vendor(&self) -> u16 {
        self.vendor
    }

    /// The PCI device / USB product id (`0` for a non-vendor key).
    #[must_use]
    pub const fn product(&self) -> u16 {
        self.product
    }

    /// The PCI/USB class code, or the virtio device id for a
    /// [`HwMatchKind::Virtio`] key.
    #[must_use]
    pub const fn class(&self) -> u32 {
        self.class
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u16(&mut out, 0, self.kind);
        out[2] = self.compatible_len;
        // out[3] reserved, already zero.
        put_u16(&mut out, 4, self.vendor);
        put_u16(&mut out, 6, self.product);
        put_u32(&mut out, 8, self.class);
        out[12..12 + HW_COMPATIBLE_MAX].copy_from_slice(&self.compatible);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is short,
    /// [`Errno::OutOfRange`] for an unknown [`HwMatchKind`], or
    /// [`Errno::LengthOutOfRange`] if `compatible_len` exceeds
    /// [`HW_COMPATIBLE_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let kind = read_u16(bytes, 0);
        if HwMatchKind::from_u16(kind).is_none() {
            return Err(Errno::OutOfRange);
        }
        let compatible_len = bytes[2];
        if usize::from(compatible_len) > HW_COMPATIBLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut compatible = [0u8; HW_COMPATIBLE_MAX];
        compatible.copy_from_slice(&bytes[12..12 + HW_COMPATIBLE_MAX]);
        Ok(Self {
            kind,
            compatible_len,
            vendor: read_u16(bytes, 4),
            product: read_u16(bytes, 6),
            class: read_u32(bytes, 8),
            compatible,
        })
    }
}

/// The kind of resource a device exposes.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum HwResourceKind {
    /// A memory-mapped register / framebuffer window (`base`..`base+len`).
    Mmio = 0,
    /// An interrupt line (`base` is the line number; `len` is the count).
    Irq = 1,
    /// An x86 programmed-I/O port range (`base` port, `len` count).
    Port = 2,
    /// A DMA capability requirement (`base`/`len` describe the addressing
    /// constraint; `0`/`0` means "no constraint declared").
    Dma = 3,
    /// An outbound bus address window with CPU↔bus translation: the CPU
    /// issues accesses in `base`..`base+len`, which the bus bridge
    /// translates to `xlate`..`xlate+len` on the far (device) side. The
    /// motivating case is a PCIe root complex's outbound `ranges`
    /// window: `base` is the CPU-physical aperture,
    /// `xlate` the PCIe-space base the bridge maps it to, distinct from
    /// a plain [`Mmio`](Self::Mmio) register window that needs no
    /// translation.
    BusWindow = 4,
}

impl HwResourceKind {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Inverse of [`Self::as_u16`]; `None` for an unknown discriminant.
    #[must_use]
    pub const fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Mmio),
            1 => Some(Self::Irq),
            2 => Some(Self::Port),
            3 => Some(Self::Dma),
            4 => Some(Self::BusWindow),
            _ => None,
        }
    }

    /// The capability a driver must hold to be granted this resource
    /// (resources are capability-grant requests, never
    /// ambient handles;).
    #[must_use]
    pub const fn required_capability(self) -> CapabilityId {
        match self {
            // A register/framebuffer window, an x86 I/O port range, and
            // an outbound bus window are all mapped through the kernel's
            // MMIO-map facility.
            Self::Mmio | Self::Port | Self::BusWindow => CapabilityId::MMIO_MAP,
            Self::Irq => CapabilityId::IRQ_BIND,
            Self::Dma => CapabilityId::MEM_DMA,
        }
    }
}

/// One resource a hardware-tree node exposes, expressed as a
/// capability-grant request.
///
/// The matched driver receives only the capability named here, scoped to
/// the `base`/`len` region — never ambient authority over the whole
/// address space or interrupt namespace. The capability is carried
/// explicitly on the wire so a consumer never has to re-derive it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HwResource {
    kind: u16,
    capability: u16,
    flags: u32,
    base: u64,
    len: u64,
    xlate: u64,
}

/// Returns `true` iff the half-open window `[child_base, child_base+child_len)`
/// lies wholly within `[parent_base, parent_base+parent_len)`.
///
/// Both ends are computed with checked arithmetic so a length that would
/// overflow the address space is refused (`false`) rather than wrapping into
/// a spuriously-contained range. A zero-length
/// child is contained as long as its base lies within the parent's closed
/// span. Used by [`HwResource::covers`] for the interval-containment kinds.
fn interval_contains(parent_base: u64, parent_len: u64, child_base: u64, child_len: u64) -> bool {
    let (Some(parent_end), Some(child_end)) = (
        parent_base.checked_add(parent_len),
        child_base.checked_add(child_len),
    ) else {
        return false;
    };
    child_base >= parent_base && child_end <= parent_end
}

impl HwResource {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 32;

    /// A memory-mapped register/framebuffer window.
    #[must_use]
    pub fn mmio(base: u64, len: u64) -> Self {
        Self::new(HwResourceKind::Mmio, base, len, 0)
    }

    /// An interrupt line (`line` number, `count` consecutive lines).
    #[must_use]
    pub fn irq(line: u64, count: u64) -> Self {
        Self::new(HwResourceKind::Irq, line, count, 0)
    }

    /// An x86 programmed-I/O port range.
    #[must_use]
    pub fn port(base: u64, count: u64) -> Self {
        Self::new(HwResourceKind::Port, base, count, 0)
    }

    /// A DMA capability requirement (`0`, `0` for "no constraint
    /// declared").
    #[must_use]
    pub fn dma(addr_limit: u64, len: u64) -> Self {
        Self::new(HwResourceKind::Dma, addr_limit, len, 0)
    }

    /// A DMA capability requirement for an inbound bus viewport that
    /// carries an address translation: `addr_limit`/`len` are the
    /// CPU-side reachability constraint (the *exclusive* upper bound and
    /// extent a device behind the bridge may reach), and `bus_base` is
    /// the far-side (bus/PCIe-space) address the viewport starts at — the
    /// inbound counterpart of [`bus_window`](Self::bus_window). The
    /// motivating case is a PCIe root complex's inbound `dma-ranges`
    /// viewport: a bus driver programs the inbound
    /// BAR from `bus_base`/`len` while the kernel bounds device DMA by
    /// `addr_limit`. Recovered through
    /// [`translated_base`](Self::translated_base).
    #[must_use]
    pub fn dma_translated(addr_limit: u64, len: u64, bus_base: u64) -> Self {
        Self::new_xlate(HwResourceKind::Dma, addr_limit, len, 0, bus_base)
    }

    /// An outbound bus address window: `cpu_base`..`cpu_base+len` on the
    /// CPU side, translated to `translated_base`..`translated_base+len`
    /// on the far (device/bus) side.
    #[must_use]
    pub fn bus_window(cpu_base: u64, len: u64, translated_base: u64) -> Self {
        Self::new_xlate(HwResourceKind::BusWindow, cpu_base, len, 0, translated_base)
    }

    fn new(kind: HwResourceKind, base: u64, len: u64, flags: u32) -> Self {
        Self::new_xlate(kind, base, len, flags, 0)
    }

    fn new_xlate(kind: HwResourceKind, base: u64, len: u64, flags: u32, xlate: u64) -> Self {
        Self {
            kind: kind.as_u16(),
            capability: kind.required_capability().as_u16(),
            flags,
            base,
            len,
            xlate,
        }
    }

    /// The kind of resource, or [`None`] if the wire discriminant is
    /// unknown.
    #[must_use]
    pub fn kind(&self) -> Option<HwResourceKind> {
        HwResourceKind::from_u16(self.kind)
    }

    /// The capability a driver must hold to be granted this resource.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if the stored capability id is out of range.
    pub fn required_capability(&self) -> Result<CapabilityId, Errno> {
        CapabilityId::from_raw(self.capability)
    }

    /// Resource base: the MMIO/port base address, or the first IRQ line.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// Resource length: the window size in bytes, the port/IRQ count, or
    /// the DMA addressing constraint.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.len
    }

    /// Implementation-defined per-resource flags (`0` today).
    #[must_use]
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// Far-side (translated) base of a window that carries a CPU↔bus
    /// address translation: the bus/device-side address `base` maps to.
    /// Set for a [`BusWindow`](HwResourceKind::BusWindow) (outbound) and
    /// for an inbound [`Dma`](HwResourceKind::Dma) viewport built with
    /// [`dma_translated`](Self::dma_translated); `0` for a plain
    /// register/port window or an untranslated DMA constraint.
    #[must_use]
    pub const fn translated_base(&self) -> u64 {
        self.xlate
    }

    /// The device-visible base address by which a driver names this
    /// resource when mapping it as a register window, or [`None`] if the
    /// resource is not a mappable register window.
    ///
    /// A plain [`Mmio`](HwResourceKind::Mmio) window lives in CPU/identity
    /// space, so it is named by its [`base`](Self::base). A
    /// [`BusWindow`](HwResourceKind::BusWindow) is addressed in outbound
    /// bus space, so it is named by its far-side
    /// [`translated_base`](Self::translated_base) — the bridge's bus→CPU
    /// translation is applied by the mapper, not the driver. Every other kind (a DMA constraint, an IRQ line, a port
    /// range) is not a mappable register window and yields [`None`].
    ///
    /// This is the single definition of "which address names this
    /// resource's register window": both
    /// [`sole_register_window`](crate::driver::sole_register_window) and a
    /// concrete driver's resource derivation build on it rather than
    /// re-deciding `base` vs `translated_base` per device class.
    #[must_use]
    pub fn register_window_base(&self) -> Option<u64> {
        match self.kind() {
            Some(HwResourceKind::Mmio) => Some(self.base),
            Some(HwResourceKind::BusWindow) => Some(self.xlate),
            _ => None,
        }
    }

    /// Returns `true` iff `self` — a device-resource grant the emitter
    /// already holds — fully covers `child`, a resource that an emitted
    /// hardware-tree node requests.
    ///
    /// This is the security spine of recursive, user-space hardware
    /// discovery: the `hw_emit_node` syscall
    /// admits a published child node **only** when every resource it
    /// requests is covered by one of the emitting bus driver's own grants,
    /// so an autoloaded child driver can never be minted more authority than
    /// the driver that discovered it (no ambient authority;
    /// — never widen a defence). It is defined once here, beside the
    /// type whose semantics it depends on, so the kernel
    /// never re-decides per-kind containment.
    ///
    /// Coverage always requires identical `flags`, and — for every pairing
    /// but one — the same [`HwResourceKind`]. Beyond that the rule follows
    /// each kind's meaning:
    ///
    /// * [`Mmio`](HwResourceKind::Mmio), [`Port`](HwResourceKind::Port), and
    ///   [`Irq`](HwResourceKind::Irq) are untranslated `[base, base+len)`
    ///   windows / line ranges: the child interval must lie wholly within
    ///   the parent's (checked arithmetic — a length overflow is refused,
    ///   never wrapped).
    /// * [`BusWindow`](HwResourceKind::BusWindow) is a translated outbound
    ///   window: a child `BusWindow` sub-window's CPU-side interval must lie
    ///   within the parent's **and** carry the identical CPU↔bus translation
    ///   delta, so it keeps the parent's addressing exactly and cannot
    ///   re-point the far side elsewhere.
    /// * **`BusWindow` parent → `Mmio` child** is the one cross-kind pairing,
    ///   and the central case of recursive PCI(e) discovery: a host bridge holds its outbound window as a `BusWindow`
    ///   grant and authorises *every* CPU access within its CPU-side
    ///   `[base, base+len)` interval. When the bridge driver enumerates a
    ///   device behind it, that device's register BAR has already been
    ///   resolved to a CPU-physical [`Mmio`](HwResourceKind::Mmio) window
    ///   inside that interval, so the bridge legitimately grants it to the
    ///   child. Coverage is exactly CPU-side containment of the child window
    ///   in the parent's — never wider (no ambient
    ///   authority): the child receives a window the bridge already owns.
    /// * [`Dma`](HwResourceKind::Dma) is an addressing *constraint* (an
    ///   exclusive address ceiling `base` and an extent `len`), not a mapped
    ///   window: the child may be no more permissive — no higher ceiling, no
    ///   larger extent, and the same bus translation.
    #[must_use]
    pub fn covers(&self, child: &HwResource) -> bool {
        let (Some(parent_kind), Some(child_kind)) = (self.kind(), child.kind()) else {
            // An undecodable discriminant on either side fails closed.
            return false;
        };
        if self.flags != child.flags {
            return false;
        }
        match (parent_kind, child_kind) {
            (HwResourceKind::Dma, HwResourceKind::Dma) => {
                self.xlate == child.xlate && child.base <= self.base && child.len <= self.len
            }
            (HwResourceKind::BusWindow, HwResourceKind::BusWindow) => {
                // The CPU↔bus translation delta must match exactly so the
                // contained sub-window maps to the contained far-side range.
                self.xlate.wrapping_sub(self.base) == child.xlate.wrapping_sub(child.base)
                    && interval_contains(self.base, self.len, child.base, child.len)
            }
            (HwResourceKind::BusWindow, HwResourceKind::Mmio) => {
                // A host bridge's outbound window covers a child register BAR
                // resolved to a CPU-physical window inside it (PCI(e)
                // discovery): pure CPU-side containment, no wider.
                interval_contains(self.base, self.len, child.base, child.len)
            }
            (HwResourceKind::Mmio, HwResourceKind::Mmio)
            | (HwResourceKind::Port, HwResourceKind::Port)
            | (HwResourceKind::Irq, HwResourceKind::Irq) => {
                interval_contains(self.base, self.len, child.base, child.len)
            }
            // Every other kind pairing fails closed.
            _ => false,
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u16(&mut out, 0, self.kind);
        put_u16(&mut out, 2, self.capability);
        put_u32(&mut out, 4, self.flags);
        put_u64(&mut out, 8, self.base);
        put_u64(&mut out, 16, self.len);
        put_u64(&mut out, 24, self.xlate);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is short, or
    /// [`Errno::OutOfRange`] for an unknown [`HwResourceKind`] or an
    /// out-of-range capability id.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let kind = read_u16(bytes, 0);
        if HwResourceKind::from_u16(kind).is_none() {
            return Err(Errno::OutOfRange);
        }
        let capability = read_u16(bytes, 2);
        CapabilityId::from_raw(capability)?;
        Ok(Self {
            kind,
            capability,
            flags: read_u32(bytes, 4),
            base: read_u64(bytes, 8),
            len: read_u64(bytes, 16),
            xlate: read_u64(bytes, 24),
        })
    }

    /// A zeroed slot, used to pad a node's fixed-size resource array.
    const EMPTY: Self = Self {
        kind: 0,
        capability: 0,
        flags: 0,
        base: 0,
        len: 0,
        xlate: 0,
    };
}

/// A kernel-issued device-resource grant delivered to a driver process:
/// the unforgeable grant handle paired with the [`HwResource`] it names.
///
/// When the kernel autoloads a driver it mints one grant per
/// [`HwResource`] the driver's matched hardware-tree node requested — and
/// no more (no ambient authority) — and hands the driver process the
/// handles. The process learns its grants through the `resource_grants`
/// syscall ([`crate::SyscallNumber::RESOURCE_GRANTS`]), which serialises
/// the task's grant set as a sequence of these records. The driver pairs
/// each handle with the [`HwResource`] it names so its
/// [`MmioMapper`](crate::MmioMapper) can resolve a requested
/// `(phys_base, len)` window to the grant that covers it before issuing
/// `mmio_map` / `dma_alloc` (the handle alone carries no description).
///
/// The wire form is the explicit little-endian byte layout
/// [`to_le_bytes`](Self::to_le_bytes) produces — the handle followed by the
/// [`HwResource`] encoding — not the in-memory struct layout, so the record
/// is endianness-stable across the user/kernel boundary exactly like
/// [`HwResource`] itself.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GrantedResource {
    /// The unforgeable, kernel-issued grant handle the `mmio_map` /
    /// `dma_alloc` syscalls resolve owner-checked against the calling task.
    /// Never the reserved `0` value (the kernel mints handles from `1`).
    pub handle: u64,
    /// The resource the handle names (a register window, an outbound bus
    /// window, an IRQ line, or a DMA constraint).
    pub resource: HwResource,
}

impl GrantedResource {
    /// Encoded size on the wire: the `u64` handle plus the [`HwResource`]
    /// encoding.
    pub const WIRE_LEN: usize = 8 + HwResource::WIRE_LEN;

    /// Pair a kernel-issued grant `handle` with the [`HwResource`] it names.
    #[must_use]
    pub const fn new(handle: u64, resource: HwResource) -> Self {
        Self { handle, resource }
    }

    /// Encode `self` little-endian: the handle at offset `0`, the
    /// [`HwResource`] encoding immediately after.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.handle);
        out[8..].copy_from_slice(&self.resource.to_le_bytes());
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is shorter than
    /// [`Self::WIRE_LEN`], or any error [`HwResource::from_bytes`] returns
    /// for the embedded resource (an unknown kind or an out-of-range
    /// capability id).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let handle = read_u64(bytes, 0);
        let resource = HwResource::from_bytes(&bytes[8..])?;
        Ok(Self { handle, resource })
    }
}

/// The result of an `msi_alloc` syscall ([`crate::SyscallNumber::MSI_ALLOC`]):
/// the kernel-allocated virtual interrupt line plus the architecture-built
/// MSI doorbell the caller writes verbatim into a PCI function's MSI
/// capability.
///
/// A bus driver that wires a PCI function for message-signalled interrupts
/// asks the kernel to allocate a vector; the kernel mints a free vector,
/// grants the caller a device resource for [`line`](Self::line) (so it may
/// both `irq_bind` it and forward it as an [`HwResource::irq`] onto a child
/// node), and reports the doorbell `(address, data)` the function's MSI
/// capability must be programmed with so its message routes to that line.
/// The doorbell is **opaque** to the driver — only the kernel's interrupt
/// controller knows what address/data its MSI controller decodes.
///
/// The wire form is the explicit little-endian byte layout
/// [`to_le_bytes`](Self::to_le_bytes) produces, so the record is
/// endianness-stable across the user/kernel boundary exactly like
/// [`GrantedResource`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MsiAllocation {
    /// The MSI doorbell target address the function's MSI capability
    /// Message-Address register is programmed with.
    pub address: u64,
    /// The MSI data word the function's MSI capability Message-Data
    /// register is programmed with (selects the vector at the controller).
    pub data: u32,
    /// The kernel virtual interrupt line the allocated vector is delivered
    /// on — what the driver `irq_bind`s, and what it forwards as an
    /// [`HwResource::irq`] onto the child node the interrupt belongs to.
    pub line: u32,
}

impl MsiAllocation {
    /// Encoded size on the wire: the `u64` address, the `u32` data, and the
    /// `u32` line.
    pub const WIRE_LEN: usize = 8 + 4 + 4;

    /// Build an allocation record from its parts.
    #[must_use]
    pub const fn new(address: u64, data: u32, line: u32) -> Self {
        Self {
            address,
            data,
            line,
        }
    }

    /// Encode `self` little-endian: the address at offset `0`, the data at
    /// `8`, the line at `12`.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.address);
        put_u32(&mut out, 8, self.data);
        put_u32(&mut out, 12, self.line);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is shorter than
    /// [`Self::WIRE_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            address: read_u64(bytes, 0),
            data: read_u32(bytes, 8),
            line: read_u32(bytes, 12),
        })
    }
}

impl HwMatchKey {
    /// A zeroed slot, used to pad a node's fixed-size match-key array.
    const EMPTY: Self = Self {
        kind: 0,
        compatible_len: 0,
        vendor: 0,
        product: 0,
        class: 0,
        compatible: [0u8; HW_COMPATIBLE_MAX],
    };
}

/// One node in the hardware tree.
///
/// A node names exactly one detected bus or device: a stable [`id`], its
/// [`parent`] ([`HW_NODE_ROOT`] for a root), a [`HwDeviceClass`], the
/// [`HwMatchKey`]s the device manager binds against, and the
/// [`HwResource`]s it exposes as capability-grant requests. The match-key
/// and resource arrays are fixed-size; the valid prefix of each is given by
/// its count.
///
/// [`id`]: Self::id
/// [`parent`]: Self::parent
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HwNode {
    id: u32,
    parent: u32,
    class: u16,
    match_key_count: u8,
    resource_count: u8,
    match_keys: [HwMatchKey; HW_NODE_MAX_MATCH_KEYS],
    resources: [HwResource; HW_NODE_MAX_RESOURCES],
}

impl HwNode {
    /// Encoded size on the wire: a 12-byte header followed by the full
    /// fixed-size match-key and resource arrays.
    pub const WIRE_LEN: usize = 12
        + HW_NODE_MAX_MATCH_KEYS * HwMatchKey::WIRE_LEN
        + HW_NODE_MAX_RESOURCES * HwResource::WIRE_LEN;

    /// Start a new node with no match keys or resources.
    #[must_use]
    pub fn new(id: u32, parent: u32, class: HwDeviceClass) -> Self {
        Self {
            id,
            parent,
            class: class.as_u16(),
            match_key_count: 0,
            resource_count: 0,
            match_keys: [HwMatchKey::EMPTY; HW_NODE_MAX_MATCH_KEYS],
            resources: [HwResource::EMPTY; HW_NODE_MAX_RESOURCES],
        }
    }

    /// Append a match key.
    ///
    /// # Errors
    ///
    /// [`Errno::NoSpace`] if the node already holds
    /// [`HW_NODE_MAX_MATCH_KEYS`] keys.
    pub fn push_match_key(&mut self, key: HwMatchKey) -> Result<(), Errno> {
        let idx = usize::from(self.match_key_count);
        if idx >= HW_NODE_MAX_MATCH_KEYS {
            return Err(Errno::NoSpace);
        }
        self.match_keys[idx] = key;
        self.match_key_count += 1;
        Ok(())
    }

    /// Append a resource request.
    ///
    /// # Errors
    ///
    /// [`Errno::NoSpace`] if the node already holds
    /// [`HW_NODE_MAX_RESOURCES`] resources.
    pub fn push_resource(&mut self, resource: HwResource) -> Result<(), Errno> {
        let idx = usize::from(self.resource_count);
        if idx >= HW_NODE_MAX_RESOURCES {
            return Err(Errno::NoSpace);
        }
        self.resources[idx] = resource;
        self.resource_count += 1;
        Ok(())
    }

    /// Stable node id.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Assign this node's kernel-owned identity: its [`id`](Self::id) and
    /// [`parent`](Self::parent).
    ///
    /// A node published into the live tree through the `hw_emit_node`
    /// syscall does **not** carry an emitter-chosen id/parent: the kernel
    /// assigns a fresh, collision-free id and sets the parent to the
    /// emitting driver's own matched node, so a driver can neither forge its
    /// position in the tree nor collide with an existing node's id
    /// (identity is kernel-provided, never
    /// caller-supplied;). The store calls this on the decoded node
    /// before it is recorded; an emitter builds the node (class, match keys,
    /// resources) and leaves the identity to the kernel.
    pub fn set_identity(&mut self, id: u32, parent: u32) {
        self.id = id;
        self.parent = parent;
    }

    /// Parent node id, or [`HW_NODE_ROOT`] for a root node.
    #[must_use]
    pub const fn parent(&self) -> u32 {
        self.parent
    }

    /// `true` if this node has no parent.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.parent == HW_NODE_ROOT
    }

    /// The node's device class, or [`None`] if the wire discriminant is
    /// unknown.
    #[must_use]
    pub fn class(&self) -> Option<HwDeviceClass> {
        HwDeviceClass::from_u16(self.class)
    }

    /// The valid prefix of the match-key array.
    #[must_use]
    pub fn match_keys(&self) -> &[HwMatchKey] {
        &self.match_keys[..usize::from(self.match_key_count)]
    }

    /// The valid prefix of the resource array.
    #[must_use]
    pub fn resources(&self) -> &[HwResource] {
        &self.resources[..usize::from(self.resource_count)]
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.id);
        put_u32(&mut out, 4, self.parent);
        put_u16(&mut out, 8, self.class);
        out[10] = self.match_key_count;
        out[11] = self.resource_count;
        let mut off = 12;
        for key in &self.match_keys {
            out[off..off + HwMatchKey::WIRE_LEN].copy_from_slice(&key.to_le_bytes());
            off += HwMatchKey::WIRE_LEN;
        }
        for resource in &self.resources {
            out[off..off + HwResource::WIRE_LEN].copy_from_slice(&resource.to_le_bytes());
            off += HwResource::WIRE_LEN;
        }
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is short,
    /// [`Errno::OutOfRange`] for an unknown [`HwDeviceClass`] or a
    /// malformed slot, or [`Errno::LengthOutOfRange`] if a count exceeds
    /// its array bound.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let class = read_u16(bytes, 8);
        if HwDeviceClass::from_u16(class).is_none() {
            return Err(Errno::OutOfRange);
        }
        let match_key_count = bytes[10];
        let resource_count = bytes[11];
        if usize::from(match_key_count) > HW_NODE_MAX_MATCH_KEYS
            || usize::from(resource_count) > HW_NODE_MAX_RESOURCES
        {
            return Err(Errno::LengthOutOfRange);
        }
        let mut match_keys = [HwMatchKey::EMPTY; HW_NODE_MAX_MATCH_KEYS];
        let mut off = 12;
        for slot in &mut match_keys {
            *slot = HwMatchKey::from_bytes(&bytes[off..off + HwMatchKey::WIRE_LEN])?;
            off += HwMatchKey::WIRE_LEN;
        }
        let mut resources = [HwResource::EMPTY; HW_NODE_MAX_RESOURCES];
        for slot in &mut resources {
            *slot = HwResource::from_bytes(&bytes[off..off + HwResource::WIRE_LEN])?;
            off += HwResource::WIRE_LEN;
        }
        Ok(Self {
            id: read_u32(bytes, 0),
            parent: read_u32(bytes, 4),
            class,
            match_key_count,
            resource_count,
            match_keys,
            resources,
        })
    }
}

/// Fixed-size header prefixing a [`crate::SyscallNumber::HW_TREE_READ`]
/// reply.
///
/// The read syscall copies out `[HwTreeHeader][HwNode; node_count]`. The
/// header tells the reader two things it cannot otherwise know from the
/// raw byte count: the store's current [`generation`](Self::generation) —
/// the value it later passes to [`crate::SyscallNumber::HW_TREE_WAIT`] to
/// block until the tree next changes — and how many
/// [`HwNode`] records follow ([`node_count`](Self::node_count)).
///
/// [`generation`]: Self::generation
/// [`node_count`]: Self::node_count
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HwTreeHeader {
    generation: u64,
    node_count: u64,
}

impl HwTreeHeader {
    /// Encoded size on the wire: two little-endian `u64`s.
    pub const WIRE_LEN: usize = 16;

    /// Build a header naming the store `generation` and the number of
    /// [`HwNode`] records that follow.
    #[must_use]
    pub const fn new(generation: u64, node_count: u64) -> Self {
        Self {
            generation,
            node_count,
        }
    }

    /// The store generation this snapshot was taken at.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The number of [`HwNode`] records following the header.
    #[must_use]
    pub const fn node_count(&self) -> u64 {
        self.node_count
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.generation);
        put_u64(&mut out, 8, self.node_count);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if the slice is shorter than
    /// [`Self::WIRE_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            generation: read_u64(bytes, 0),
            node_count: read_u64(bytes, 8),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_root_sentinel_are_frozen() {
        assert_eq!(HWTREE_VERSION_V1, 1);
        assert_eq!(HW_NODE_ROOT, u32::MAX);
    }

    #[test]
    fn device_class_round_trips_and_rejects_unknown() {
        for class in [
            HwDeviceClass::Root,
            HwDeviceClass::Bus,
            HwDeviceClass::Cpu,
            HwDeviceClass::Memory,
            HwDeviceClass::Timer,
            HwDeviceClass::InterruptController,
            HwDeviceClass::Display,
            HwDeviceClass::Input,
            HwDeviceClass::Network,
            HwDeviceClass::Storage,
            HwDeviceClass::Serial,
            HwDeviceClass::Other,
        ] {
            assert_eq!(HwDeviceClass::from_u16(class.as_u16()), Some(class));
        }
        assert_eq!(HwDeviceClass::from_u16(11), None);
        assert_eq!(HwDeviceClass::from_u16(64_000), None);
        assert_eq!(HwDeviceClass::default(), HwDeviceClass::Root);
    }

    #[test]
    fn match_kind_round_trips_and_rejects_unknown() {
        for kind in [
            HwMatchKind::Compatible,
            HwMatchKind::Pci,
            HwMatchKind::Usb,
            HwMatchKind::Virtio,
        ] {
            assert_eq!(HwMatchKind::from_u16(kind.as_u16()), Some(kind));
        }
        assert_eq!(HwMatchKind::from_u16(4), None);
    }

    #[test]
    fn compatible_match_key_round_trips() {
        let key = HwMatchKey::compatible(b"riscv,clint0").expect("fits");
        assert_eq!(key.kind(), Some(HwMatchKind::Compatible));
        assert_eq!(key.compatible_bytes(), b"riscv,clint0");
        let bytes = key.to_le_bytes();
        assert_eq!(bytes.len(), HwMatchKey::WIRE_LEN);
        let back = HwMatchKey::from_bytes(&bytes).expect("decode");
        assert_eq!(back, key);
    }

    #[test]
    fn numeric_match_keys_round_trip() {
        let pci = HwMatchKey::pci(0x1af4, 0x1000, 0x0002_0000);
        assert_eq!(pci.kind(), Some(HwMatchKind::Pci));
        assert_eq!(pci.vendor(), 0x1af4);
        assert_eq!(pci.product(), 0x1000);
        assert_eq!(pci.class(), 0x0002_0000);
        assert_eq!(HwMatchKey::from_bytes(&pci.to_le_bytes()).unwrap(), pci);

        let usb = HwMatchKey::usb(0x046d, 0xc52b, 0x03);
        assert_eq!(usb.kind(), Some(HwMatchKind::Usb));
        assert_eq!(HwMatchKey::from_bytes(&usb.to_le_bytes()).unwrap(), usb);

        let virtio = HwMatchKey::virtio(2);
        assert_eq!(virtio.kind(), Some(HwMatchKind::Virtio));
        assert_eq!(virtio.class(), 2);
        assert_eq!(virtio.compatible_bytes(), b"");
        assert_eq!(
            HwMatchKey::from_bytes(&virtio.to_le_bytes()).unwrap(),
            virtio
        );
    }

    #[test]
    fn match_key_rejects_overlong_compatible() {
        let too_long = [b'a'; HW_COMPATIBLE_MAX + 1];
        assert_eq!(
            HwMatchKey::compatible(&too_long),
            Err(Errno::LengthOutOfRange)
        );
        // The boundary length is accepted.
        let exact = [b'a'; HW_COMPATIBLE_MAX];
        assert!(HwMatchKey::compatible(&exact).is_ok());
    }

    #[test]
    fn match_key_decode_rejects_short_and_bad_kind() {
        assert_eq!(HwMatchKey::from_bytes(&[]), Err(Errno::BufferTooSmall));
        let mut bytes = HwMatchKey::pci(1, 2, 3).to_le_bytes();
        put_u16(&mut bytes, 0, 99);
        assert_eq!(HwMatchKey::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn register_window_base_selects_the_addressing_space_per_kind() {
        // An MMIO window is named by its CPU base.
        assert_eq!(
            HwResource::mmio(0x1000_0000, 0x1000).register_window_base(),
            Some(0x1000_0000)
        );
        // A bus window is named by its far-side (translated) base.
        assert_eq!(
            HwResource::bus_window(0x6_0000_0000, 0x2000, 0x4000_0000).register_window_base(),
            Some(0x4000_0000)
        );
        // Non-window resources are not mappable register windows.
        assert_eq!(HwResource::dma(0x8000_0000, 0).register_window_base(), None);
        assert_eq!(HwResource::irq(33, 1).register_window_base(), None);
        assert_eq!(HwResource::port(0x60, 8).register_window_base(), None);
    }

    #[test]
    fn bind_key_matches_exactly_and_by_wildcard() {
        // A fully-specified PCI bind key matches only its exact device.
        let exact = HwMatchKey::pci(0x1106, 0x3483, 0x0C_0330);
        let vl805 = HwMatchKey::pci(0x1106, 0x3483, 0x0C_0330);
        let other_vendor = HwMatchKey::pci(0x8086, 0x3483, 0x0C_0330);
        assert!(exact.matches(&vl805));
        assert!(!exact.matches(&other_vendor));

        // A class-wildcard bind key (vendor/device 0) binds any xHCI host,
        // whatever its vendor/device, but not a different class.
        let any_xhci = HwMatchKey::pci(0, 0, 0x0C_0330);
        assert!(any_xhci.matches(&vl805));
        assert!(any_xhci.matches(&other_vendor));
        assert!(!any_xhci.matches(&HwMatchKey::pci(0x1106, 0x3483, 0x0C_0300)));

        // The wildcard is one-directional: a concrete *device* key never
        // widens — a device advertising vendor 0 does not match a bind key
        // demanding a specific vendor.
        assert!(!exact.matches(&HwMatchKey::pci(0, 0, 0x0C_0330)));
    }

    #[test]
    fn bind_key_match_respects_kind_and_compatible_bytes() {
        // Different kinds never match, even with equal numeric payloads.
        let pci = HwMatchKey::pci(0, 0, 7);
        let usb = HwMatchKey::usb(0, 0, 7);
        assert!(!pci.matches(&usb));
        assert!(!usb.matches(&pci));

        // Compatible keys match byte-for-byte only.
        let a = HwMatchKey::compatible(b"brcm,bcm2711-pcie").unwrap();
        let same = HwMatchKey::compatible(b"brcm,bcm2711-pcie").unwrap();
        let diff = HwMatchKey::compatible(b"brcm,bcm2711-emmc2").unwrap();
        assert!(a.matches(&same));
        assert!(!a.matches(&diff));

        // Virtio keys match on device id; a USB HID boot pair is selected
        // per-protocol.
        assert!(HwMatchKey::virtio(2).matches(&HwMatchKey::virtio(2)));
        assert!(!HwMatchKey::virtio(2).matches(&HwMatchKey::virtio(1)));
        let kbd = HwMatchKey::usb(0, 0, 0x03_01_01);
        assert!(kbd.matches(&HwMatchKey::usb(0x046D, 0xC52B, 0x03_01_01)));
        assert!(!kbd.matches(&HwMatchKey::usb(0x046D, 0xC52B, 0x03_01_02)));
    }

    #[test]
    fn resource_capability_mapping_is_correct() {
        assert_eq!(
            HwResourceKind::Mmio.required_capability(),
            CapabilityId::MMIO_MAP
        );
        assert_eq!(
            HwResourceKind::Port.required_capability(),
            CapabilityId::MMIO_MAP
        );
        assert_eq!(
            HwResourceKind::Irq.required_capability(),
            CapabilityId::IRQ_BIND
        );
        assert_eq!(
            HwResourceKind::Dma.required_capability(),
            CapabilityId::MEM_DMA
        );
        assert_eq!(
            HwResourceKind::BusWindow.required_capability(),
            CapabilityId::MMIO_MAP
        );
    }

    #[test]
    fn bus_window_carries_its_translation() {
        // The Pi 4 outbound `ranges`: CPU 0x6_0000_0000 -> PCIe
        // 0xc000_0000, 1 GiB. The CPU base, length, and far-side base all
        // survive the round trip, and only this kind carries a non-zero
        // translation.
        let win = HwResource::bus_window(0x6_0000_0000, 0x4000_0000, 0xc000_0000);
        assert_eq!(win.kind(), Some(HwResourceKind::BusWindow));
        assert_eq!(win.base(), 0x6_0000_0000);
        assert_eq!(win.length(), 0x4000_0000);
        assert_eq!(win.translated_base(), 0xc000_0000);
        assert_eq!(win.required_capability(), Ok(CapabilityId::MMIO_MAP));
        let back = HwResource::from_bytes(&win.to_le_bytes()).expect("decode");
        assert_eq!(back, win);
        assert_eq!(back.translated_base(), 0xc000_0000);
        // A plain MMIO window has no translation.
        assert_eq!(HwResource::mmio(0x1000, 0x1000).translated_base(), 0);
    }

    #[test]
    fn translated_dma_viewport_carries_its_bus_base() {
        // The Pi 4 inbound `dma-ranges`: PCIe base 0 views system memory,
        // CPU reachability bounded at 0xc000_0000 (the low 3 GiB), 3 GiB
        // extent. The reachability bound is `base`/`len` (so the existing
        // DMA consumer is unchanged) and the far-side PCIe base rides
        // `translated_base`.
        let dma = HwResource::dma_translated(0xc000_0000, 0xc000_0000, 0);
        assert_eq!(dma.kind(), Some(HwResourceKind::Dma));
        assert_eq!(dma.base(), 0xc000_0000);
        assert_eq!(dma.length(), 0xc000_0000);
        assert_eq!(dma.translated_base(), 0);
        assert_eq!(dma.required_capability(), Ok(CapabilityId::MEM_DMA));
        let back = HwResource::from_bytes(&dma.to_le_bytes()).expect("decode");
        assert_eq!(back, dma);

        // A non-zero far-side base survives too (a viewport not anchored
        // at PCIe address 0).
        let offset = HwResource::dma_translated(0x8000_0000, 0x8000_0000, 0x4000_0000);
        assert_eq!(offset.translated_base(), 0x4000_0000);
        assert_eq!(
            HwResource::from_bytes(&offset.to_le_bytes()).unwrap(),
            offset
        );
        // An untranslated DMA constraint still reads back a zero far-side.
        assert_eq!(HwResource::dma(0, 0).translated_base(), 0);
    }

    #[test]
    fn resource_round_trips_and_carries_capability() {
        let mmio = HwResource::mmio(0x1000_0000, 0x1000);
        assert_eq!(mmio.kind(), Some(HwResourceKind::Mmio));
        assert_eq!(mmio.base(), 0x1000_0000);
        assert_eq!(mmio.length(), 0x1000);
        assert_eq!(mmio.required_capability(), Ok(CapabilityId::MMIO_MAP));
        let back = HwResource::from_bytes(&mmio.to_le_bytes()).expect("decode");
        assert_eq!(back, mmio);
        assert_eq!(back.required_capability(), Ok(CapabilityId::MMIO_MAP));

        let irq = HwResource::irq(33, 1);
        assert_eq!(irq.required_capability(), Ok(CapabilityId::IRQ_BIND));
        assert_eq!(HwResource::from_bytes(&irq.to_le_bytes()).unwrap(), irq);

        let dma = HwResource::dma(0, 0);
        assert_eq!(dma.required_capability(), Ok(CapabilityId::MEM_DMA));
        assert_eq!(HwResource::from_bytes(&dma.to_le_bytes()).unwrap(), dma);
    }

    #[test]
    fn resource_decode_rejects_short_and_bad_kind() {
        assert_eq!(
            HwResource::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        let mut bytes = HwResource::mmio(0, 0).to_le_bytes();
        put_u16(&mut bytes, 0, 9);
        assert_eq!(HwResource::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn covers_accepts_a_contained_sub_window_and_rejects_escapes() {
        // An MMIO grant covers a sub-window inside it, the whole window, and
        // a zero-length probe at its base, but not a window that starts
        // below it, extends past its end, or is a different kind.
        let parent = HwResource::mmio(0x1000_0000, 0x1_0000);
        assert!(parent.covers(&HwResource::mmio(0x1000_0000, 0x1_0000)));
        assert!(parent.covers(&HwResource::mmio(0x1000_1000, 0x1000)));
        assert!(parent.covers(&HwResource::mmio(0x1000_0000, 0)));
        assert!(!parent.covers(&HwResource::mmio(0x0FFF_F000, 0x1000)));
        assert!(!parent.covers(&HwResource::mmio(0x1000_F000, 0x2000)));
        assert!(!parent.covers(&HwResource::irq(0x1000_0000, 1)));
        // A length that would overflow the address space is refused, never
        // wrapped into a spuriously-contained range.
        assert!(!parent.covers(&HwResource::mmio(0x1000_0000, u64::MAX)));
    }

    #[test]
    fn covers_requires_a_matching_bus_window_translation() {
        // An outbound bus window covers a CPU-side sub-window that keeps the
        // identical CPU↔bus translation delta, but not one that re-points the
        // far side (a child cannot reach a different bus address).
        let parent = HwResource::bus_window(0x6_0000_0000, 0x400_0000, 0xF800_0000);
        assert!(parent.covers(&HwResource::bus_window(
            0x6_0000_0000,
            0x10_0000,
            0xF800_0000
        )));
        // Same delta (cpu and bus both advanced by 0x1000) -> still covered.
        assert!(parent.covers(&HwResource::bus_window(0x6_0000_1000, 0x1000, 0xF800_1000)));
        // Same CPU sub-window but a re-pointed far side -> rejected.
        assert!(!parent.covers(&HwResource::bus_window(0x6_0000_0000, 0x1000, 0xF900_0000)));
    }

    #[test]
    fn covers_lets_a_bridge_window_cover_a_child_bar_inside_it() {
        // The central recursive-PCI(e) case: a host
        // bridge holds its outbound window as a `BusWindow` grant and grants
        // an enumerated device's register BAR — a CPU-physical `Mmio` window
        // the bridge already owns — to the child driver. Coverage is exactly
        // CPU-side containment of the BAR in the bridge's CPU window.
        let bridge = HwResource::bus_window(0x6_0000_0000, 0x400_0000, 0xF800_0000);
        // A BAR resolved to a CPU window inside the bridge's outbound window
        // is covered (the whole window, a sub-window, a zero-length probe).
        assert!(bridge.covers(&HwResource::mmio(0x6_0000_0000, 0x400_0000)));
        assert!(bridge.covers(&HwResource::mmio(0x6_0010_0000, 0x1_0000)));
        assert!(bridge.covers(&HwResource::mmio(0x6_0000_0000, 0)));
        // A BAR that starts below the bridge window or runs past its end is
        // never minted to the child (no ambient authority).
        assert!(!bridge.covers(&HwResource::mmio(0x5_FFFF_F000, 0x1000)));
        assert!(!bridge.covers(&HwResource::mmio(0x6_03FF_F000, 0x2000)));
        // A length that would overflow the address space is refused, never
        // wrapped into a spuriously-contained range.
        assert!(!bridge.covers(&HwResource::mmio(0x6_0000_0000, u64::MAX)));
        // The reverse direction is NOT symmetric: a plain MMIO grant never
        // confers a translating bus window on a child (fails closed).
        let plain = HwResource::mmio(0x6_0000_0000, 0x400_0000);
        assert!(!plain.covers(&HwResource::bus_window(0x6_0000_0000, 0x1000, 0xF800_0000)));
        // Nor does a bridge window confer a port range or an IRQ line on a
        // child by mere numeric containment (only the BAR/`Mmio` pairing).
        assert!(!bridge.covers(&HwResource::port(0x6_0000_0000, 0x10)));
        assert!(!bridge.covers(&HwResource::irq(0x6_0000_0000, 1)));
    }

    #[test]
    fn covers_treats_dma_as_a_no_wider_constraint() {
        // A DMA grant covers a child with no higher ceiling, no larger
        // extent, and the same translation; anything more permissive fails.
        let parent = HwResource::dma_translated(0xC000_0000, 0x4000_0000, 0x0);
        assert!(parent.covers(&HwResource::dma_translated(0xC000_0000, 0x4000_0000, 0x0)));
        assert!(parent.covers(&HwResource::dma_translated(0xB000_0000, 0x1000_0000, 0x0)));
        // Higher ceiling -> rejected.
        assert!(!parent.covers(&HwResource::dma_translated(0xC100_0000, 0x1000, 0x0)));
        // Larger extent -> rejected.
        assert!(!parent.covers(&HwResource::dma_translated(0xC000_0000, 0x5000_0000, 0x0)));
        // Different translation -> rejected.
        assert!(!parent.covers(&HwResource::dma_translated(0xC000_0000, 0x1000, 0x1_0000)));
    }

    #[test]
    fn granted_resource_round_trips() {
        // A register-window grant: handle + the embedded HwResource decode
        // back identically (the `resource_grants` delivery contract).
        let grant = GrantedResource::new(7, HwResource::mmio(0xFD50_0000, 0x9310));
        let bytes = grant.to_le_bytes();
        assert_eq!(bytes.len(), GrantedResource::WIRE_LEN);
        let back = GrantedResource::from_bytes(&bytes).expect("decode");
        assert_eq!(back, grant);
        assert_eq!(back.handle, 7);
        assert_eq!(
            back.resource.required_capability(),
            Ok(CapabilityId::MMIO_MAP)
        );

        // A translating outbound bus-window grant preserves its far-side base.
        let win = GrantedResource::new(
            2,
            HwResource::bus_window(0x6_0000_0000, 0x400_0000, 0xF800_0000),
        );
        let back = GrantedResource::from_bytes(&win.to_le_bytes()).expect("decode");
        assert_eq!(back, win);
        assert_eq!(back.resource.translated_base(), 0xF800_0000);
    }

    #[test]
    fn granted_resource_decode_rejects_short_and_bad_resource() {
        // Too short to hold a whole record.
        assert_eq!(
            GrantedResource::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
        // A well-sized buffer whose embedded resource carries an unknown kind
        // is rejected by `HwResource::from_bytes`.
        let mut bytes = GrantedResource::new(1, HwResource::mmio(0, 0)).to_le_bytes();
        put_u16(&mut bytes, 8, 9); // unknown HwResourceKind at the resource's offset
        assert_eq!(GrantedResource::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    fn sample_node() -> HwNode {
        let mut node = HwNode::new(7, 0, HwDeviceClass::Network);
        node.push_match_key(HwMatchKey::pci(0x1af4, 0x1000, 0x0002_0000))
            .unwrap();
        node.push_match_key(HwMatchKey::virtio(1)).unwrap();
        node.push_resource(HwResource::mmio(0x4000_0000, 0x1000))
            .unwrap();
        node.push_resource(HwResource::irq(34, 1)).unwrap();
        node
    }

    #[test]
    fn node_round_trips() {
        let node = sample_node();
        assert_eq!(node.id(), 7);
        assert_eq!(node.parent(), 0);
        assert!(!node.is_root());
        assert_eq!(node.class(), Some(HwDeviceClass::Network));
        assert_eq!(node.match_keys().len(), 2);
        assert_eq!(node.resources().len(), 2);

        let bytes = node.to_le_bytes();
        assert_eq!(bytes.len(), HwNode::WIRE_LEN);
        let back = HwNode::from_bytes(&bytes).expect("decode");
        assert_eq!(back, node);
        assert_eq!(back.match_keys()[0].vendor(), 0x1af4);
        assert_eq!(
            back.resources()[1].required_capability(),
            Ok(CapabilityId::IRQ_BIND)
        );
    }

    #[test]
    fn root_node_is_detected() {
        let root = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root);
        assert!(root.is_root());
        assert_eq!(
            HwNode::from_bytes(&root.to_le_bytes()).unwrap().parent(),
            HW_NODE_ROOT
        );
    }

    #[test]
    fn node_push_is_bounded() {
        let mut node = HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Bus);
        for _ in 0..HW_NODE_MAX_MATCH_KEYS {
            node.push_match_key(HwMatchKey::virtio(1)).unwrap();
        }
        assert_eq!(
            node.push_match_key(HwMatchKey::virtio(1)),
            Err(Errno::NoSpace)
        );
        for _ in 0..HW_NODE_MAX_RESOURCES {
            node.push_resource(HwResource::irq(1, 1)).unwrap();
        }
        assert_eq!(
            node.push_resource(HwResource::irq(1, 1)),
            Err(Errno::NoSpace)
        );
    }

    #[test]
    fn node_decode_rejects_short_bad_class_and_overlong_counts() {
        assert_eq!(HwNode::from_bytes(&[0u8; 4]), Err(Errno::BufferTooSmall));

        let mut bytes = sample_node().to_le_bytes();
        put_u16(&mut bytes, 8, 11); // unknown device class
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::OutOfRange));

        let mut bytes = sample_node().to_le_bytes();
        bytes[10] = u8::try_from(HW_NODE_MAX_MATCH_KEYS + 1).unwrap();
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::LengthOutOfRange));

        let mut bytes = sample_node().to_le_bytes();
        bytes[11] = u8::try_from(HW_NODE_MAX_RESOURCES + 1).unwrap();
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn wire_lengths_are_frozen() {
        // Pinned so an accidental layout change is caught.
        assert_eq!(HwMatchKey::WIRE_LEN, 76);
        assert_eq!(HwResource::WIRE_LEN, 32);
        assert_eq!(GrantedResource::WIRE_LEN, 40);
        assert_eq!(HwNode::WIRE_LEN, 572);
        assert_eq!(HwTreeHeader::WIRE_LEN, 16);
    }

    #[test]
    fn tree_header_round_trips() {
        let header = HwTreeHeader::new(0x1122_3344_5566_7788, 3);
        assert_eq!(header.generation(), 0x1122_3344_5566_7788);
        assert_eq!(header.node_count(), 3);

        let bytes = header.to_le_bytes();
        assert_eq!(bytes.len(), HwTreeHeader::WIRE_LEN);
        assert_eq!(HwTreeHeader::from_bytes(&bytes), Ok(header));
    }

    #[test]
    fn tree_header_decode_rejects_short() {
        assert_eq!(
            HwTreeHeader::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }
}
