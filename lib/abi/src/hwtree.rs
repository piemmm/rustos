//! The architecture-neutral hardware tree (`AGENTS.md` §18.1).
//!
//! RustOS detects the hardware actually present at boot and autoloads the
//! matching drivers; it does not ship a hand-maintained static device list
//! (`AGENTS.md` §18). The *single* inventory contract is the **hardware
//! tree** defined here: each architecture port normalises its platform's
//! native source (ACPI on x86_64, a flattened device tree on aarch64 /
//! riscv64, a host-capability query on wasm32) into a flat list of
//! [`HwNode`]s linked by parent id, and the user-space device manager
//! (`userland/system/devmgr`) matches each node's [`HwMatchKey`]s against
//! the bind table every driver declares in its signed manifest.
//!
//! # ABI discipline
//!
//! The hardware tree is held to the same discipline as the syscall table
//! (`AGENTS.md` §9) and the System Information API (§16.6): it is
//! versioned ([`HWTREE_VERSION_V1`]), every record has a fixed wire layout
//! pinned by a `WIRE_LEN` constant and a frozen-layout host test, and the
//! C view is generated from this source of truth (`cargo xtask c-header`).
//! Extend the tree with a new version; never mutate a shipped one.
//!
//! # No ambient authority
//!
//! A node's resources (MMIO windows, IRQ lines, port ranges, DMA needs)
//! are expressed as capability-grant **requests** ([`HwResource`]), never
//! as raw ambient handles (`AGENTS.md` §4): a matched driver receives only
//! the resource capabilities its node requested, and no more (§18.3). The
//! capability a resource needs is named explicitly as a [`CapabilityId`].
//!
//! The types are `#[repr(C)]`, `no_std`, and allocation-free: a node is a
//! fixed-size record built on the boot stack, encoded little-endian through
//! the shared `le` helpers, and decoded with every field bounds-checked
//! against `WIRE_LEN` (`AGENTS.md` §5.4 — validate every input, fail
//! closed).

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::{CapabilityId, Errno};

/// Hardware-tree ABI version tag.
///
/// Carried in every serialised tree so a consumer can refuse a tree
/// produced for a future revision rather than misinterpreting it. Frozen
/// for `abi-v1`; new behaviour bumps the version (`AGENTS.md` §18.1).
pub const HWTREE_VERSION_V1: u16 = 1;

/// Sentinel parent id marking a node with no parent (a tree root).
///
/// A real node id is a small dense index; `u32::MAX` can never collide
/// with one, so it is the unambiguous "no parent" marker.
pub const HW_NODE_ROOT: u32 = u32::MAX;

/// Maximum bytes of a device-tree / MMIO `compatible` string a match key
/// carries inline. Longer strings are rejected, never truncated.
pub const HW_COMPATIBLE_MAX: usize = 64;

/// Maximum number of [`HwMatchKey`]s a single node carries.
pub const HW_NODE_MAX_MATCH_KEYS: usize = 4;

/// Maximum number of [`HwResource`]s a single node carries.
pub const HW_NODE_MAX_RESOURCES: usize = 8;

/// Device class of a hardware-tree node.
///
/// A closed set matching the driver folder classes (`AGENTS.md` §3 /
/// §18.1) plus the structural classes the discovery code needs to model a
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
/// declares in its signed manifest (`AGENTS.md` §18.3). A key is either a
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
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `compatible` exceeds
    /// [`HW_COMPATIBLE_MAX`]; the string is never truncated.
    pub fn compatible(compatible: &[u8]) -> Result<Self, Errno> {
        if compatible.len() > HW_COMPATIBLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; HW_COMPATIBLE_MAX];
        buf[..compatible.len()].copy_from_slice(compatible);
        let compatible_len = u8::try_from(compatible.len()).map_err(|_| Errno::LengthOutOfRange)?;
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
    #[must_use]
    pub fn pci(vendor: u16, device: u16, class: u32) -> Self {
        Self::numeric(HwMatchKind::Pci, vendor, device, class)
    }

    /// A USB `vid:pid:class` match key.
    #[must_use]
    pub fn usb(vendor: u16, product: u16, class: u32) -> Self {
        Self::numeric(HwMatchKind::Usb, vendor, product, class)
    }

    /// A virtio device-id match key.
    #[must_use]
    pub fn virtio(device_id: u32) -> Self {
        Self::numeric(HwMatchKind::Virtio, 0, 0, device_id)
    }

    fn numeric(kind: HwMatchKind, vendor: u16, product: u16, class: u32) -> Self {
        Self {
            kind: kind.as_u16(),
            compatible_len: 0,
            vendor,
            product,
            class,
            compatible: [0u8; HW_COMPATIBLE_MAX],
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
    /// window (`AGENTS.md` §18.1): `base` is the CPU-physical aperture,
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
    /// (`AGENTS.md` §4 — resources are capability-grant requests, never
    /// ambient handles; §18.3).
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
/// capability-grant request (`AGENTS.md` §4 / §18.1).
///
/// The matched driver receives only the capability named here, scoped to
/// the `base`/`len` region — never ambient authority over the whole
/// address space or interrupt namespace (§18.3). The capability is carried
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

    /// An outbound bus address window: `cpu_base`..`cpu_base+len` on the
    /// CPU side, translated to `translated_base`..`translated_base+len`
    /// on the far (device/bus) side (`AGENTS.md` §18.1).
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

    /// Far-side (translated) base of a [`BusWindow`](HwResourceKind::BusWindow):
    /// the address `base` maps to on the device/bus side. `0` for every
    /// other resource kind, which needs no translation.
    #[must_use]
    pub const fn translated_base(&self) -> u64 {
        self.xlate
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

/// One node in the hardware tree (`AGENTS.md` §18.1).
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
        // Pinned so an accidental layout change is caught (AGENTS.md §9).
        assert_eq!(HwMatchKey::WIRE_LEN, 76);
        assert_eq!(HwResource::WIRE_LEN, 32);
        assert_eq!(HwNode::WIRE_LEN, 572);
    }
}
