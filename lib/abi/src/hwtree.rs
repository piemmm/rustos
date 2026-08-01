//! The architecture-neutral hardware tree.
//!
//! TAIRiX detects the hardware actually present at boot and autoloads the
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

use crate::blkio::{BlkStatus, FaultDomainState};
use crate::driver::display::{DisplayFormat, DisplayMode};
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

/// The `compatible` string of the synthetic **virtual bus** the kernel
/// publishes directly beneath the root on every machine.
///
/// Firmware describes only physical devices, so a *composed* block device —
/// a RAID array assembled from several member disks, a future
/// device-mapper-style volume — has no discovered node to hang from and no
/// parent whose lifetime outlives the devices it is built out of. The
/// virtual bus is that parent: an always-present container, exactly like
/// Linux's `virtual` bus, whose lifetime is the machine's rather than any
/// disk's, so pulling a member can never orphan the array node built above
/// it.
///
/// It describes no hardware and asserts nothing about the machine, so it is
/// not a static device list standing in for detection: it is a fixed
/// structural feature of the tree, like the root itself. The driver matched
/// to it composes devices it is *given*; it discovers nothing from the
/// node's existence.
///
/// The string lives here because the kernel publishes the node and a
/// user-space driver binds it, and neither may depend on the other.
pub const HW_VIRTUAL_BUS_COMPATIBLE: &[u8] = b"tairix,virtual-bus";

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
    /// A synchronous IPC **call endpoint** a driver may submit to: `base`
    /// is the endpoint id, `len` is `1` (a single endpoint). It is not a
    /// memory window or an interrupt line but the right to `ipc_call` one
    /// grant-restricted endpoint a server created (the USB request-block
    /// transport one host-controller driver serves per device it
    /// enumerates, `plans/USB.md`). The matched class driver receives this
    /// resource as its sole grant, so it can reach exactly its own
    /// interface's endpoint and nothing else (no ambient authority).
    Endpoint = 5,
    /// A cross-process **shared-memory region** the holder may map: `base`
    /// is the shared-region id, `len` is `1` (a single region). It is not a
    /// memory window of fixed physical hardware but the right to map one
    /// kernel-owned region a server created (the USB request-block transport
    /// data buffer one host-controller driver serves per device it
    /// enumerates, `plans/USB.md`). The matched class driver receives this
    /// resource as its sole grant, so it can map exactly its own interface's
    /// buffer and nothing else (no ambient authority).
    Shared = 6,
    /// A **linear scan-out surface**: a mappable framebuffer window
    /// (`base`..`base+len`) that additionally carries the pixel geometry
    /// the platform programmed it with, so a display driver spawned into
    /// user space learns its mode from discovery rather than a board
    /// constant — the FDT `simple-framebuffer` model, normalised into the
    /// hardware tree (`plans/DISPLAY.md` D7b). The per-kind fields:
    /// `flags` is the [`DisplayFormat`] wire value (its reserved high
    /// bits zero), `xlate` packs the width (low 32 bits) and height
    /// (high 32 bits) in pixels, and the stride is `len / height`,
    /// recovered and validated by
    /// [`HwResource::framebuffer_mode`]. A GPU that exposes an
    /// accelerated engine publishes its own register/DMA resources
    /// alongside — this kind describes only the dumb linear surface.
    Framebuffer = 7,
}

/// CPU mapping policy for a linear framebuffer resource.
///
/// The platform discovery path reports the policy from the surface's actual
/// backing: emulated scan-out over ordinary coherent RAM is [`WriteBack`](Self::WriteBack),
/// while a CPU-written display aperture is [`WriteCombine`](Self::WriteCombine).
/// Drivers never guess from an address or a board name.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum FramebufferMemory {
    /// Ordinary coherent RAM, mapped with the architecture's normal
    /// write-back policy.
    WriteBack = 1,
    /// A write-mostly display aperture, mapped with the architecture's
    /// write-combining policy.
    WriteCombine = 2,
}

impl FramebufferMemory {
    const fn as_u8(self) -> u8 {
        self as u8
    }

    const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::WriteBack),
            2 => Ok(Self::WriteCombine),
            _ => Err(Errno::OutOfRange),
        }
    }
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
            5 => Some(Self::Endpoint),
            6 => Some(Self::Shared),
            7 => Some(Self::Framebuffer),
            _ => None,
        }
    }

    /// The capability a driver must hold to be granted this resource
    /// (resources are capability-grant requests, never
    /// ambient handles;).
    #[must_use]
    pub const fn required_capability(self) -> CapabilityId {
        match self {
            // A register/framebuffer window (plain or geometry-carrying),
            // an x86 I/O port range, and an outbound bus window are all
            // mapped through the kernel's MMIO-map facility.
            Self::Mmio | Self::Port | Self::BusWindow | Self::Framebuffer => CapabilityId::MMIO_MAP,
            Self::Irq => CapabilityId::IRQ_BIND,
            Self::Dma => CapabilityId::MEM_DMA,
            // Submitting to a grant-restricted call endpoint is gated by the
            // generic per-endpoint call-IPC capability; the per-endpoint
            // grant (this resource) scopes it to one endpoint id.
            Self::Endpoint => CapabilityId::IPC_ENDPOINT,
            // Mapping a granted shared-memory region is gated by the generic
            // shared-memory capability; the per-region grant (this resource)
            // scopes it to one region id.
            Self::Shared => CapabilityId::SHM,
        }
    }
}

/// Device-tree `compatible` model name of a firmware/platform-programmed
/// linear scan-out surface (the FDT `simple-framebuffer` binding).
///
/// The single definition shared by the platform discovery that publishes
/// a boot display node carrying a [`HwResourceKind::Framebuffer`]
/// resource and the display-service driver's bind table
/// (`drivers/display/framebuffer`), so the emitted match key and the
/// driver's `BIND_KEYS` can never drift.
pub const SIMPLE_FRAMEBUFFER_COMPATIBLE: &[u8] = b"simple-framebuffer";

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

    const FRAMEBUFFER_FORMAT_MASK: u32 = 0x0000_00FF;
    const FRAMEBUFFER_MEMORY_MASK: u32 = 0x0000_FF00;
    const FRAMEBUFFER_MEMORY_SHIFT: u32 = 8;
    const FRAMEBUFFER_FLAGS_MASK: u32 =
        Self::FRAMEBUFFER_FORMAT_MASK | Self::FRAMEBUFFER_MEMORY_MASK;

    /// A memory-mapped register/framebuffer window.
    #[must_use]
    pub fn mmio(base: u64, len: u64) -> Self {
        Self::new(HwResourceKind::Mmio, base, len, 0)
    }

    /// A memory-mapped register window carrying a consumer-interpreted
    /// `tag` and an auxiliary datum `aux`.
    ///
    /// Mapped exactly like [`mmio`](Self::mmio) — a driver names the
    /// window by its [`base`](Self::base) and the kernel maps it under
    /// `CAP_MMIO_MAP` — but it additionally carries two opaque fields the
    /// granting bus and the matched driver agree on, so a device that
    /// exposes several windows can hand them all to one driver without
    /// the driver re-reading the bus's configuration space:
    ///
    /// * `tag` (read back through [`flags`](Self::flags)) labels which
    ///   window this is. The modern virtio-PCI transport tags its four
    ///   config windows by `cfg_type`
    ///   ([`crate::driver::virtio_pci`]).
    /// * `aux` (read back through [`translated_base`](Self::translated_base),
    ///   which a plain register window does not otherwise use) carries one
    ///   window-specific 64-bit datum — for the virtio notify window, its
    ///   `notify_off_multiplier`.
    ///
    /// Both fields are opaque to the kernel and to the mapping path; only
    /// the emitting bus and the consuming driver interpret them, so this
    /// stays platform-neutral.
    #[must_use]
    pub fn mmio_tagged(base: u64, len: u64, tag: u32, aux: u64) -> Self {
        Self::new_xlate(HwResourceKind::Mmio, base, len, tag, aux)
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

    /// A synchronous IPC call endpoint the holder may submit to (`id` is
    /// the grant-restricted call-endpoint id). The grant covers exactly the
    /// one endpoint: a host-controller driver mints it for itself when it
    /// creates the endpoint, forwards it onto the per-device node it emits,
    /// and the autoloaded class driver inherits it as its sole reach.
    #[must_use]
    pub fn endpoint(id: u64) -> Self {
        Self::new(HwResourceKind::Endpoint, id, 1, 0)
    }

    /// A cross-process shared-memory region the holder may map (`id` is the
    /// shared-region id). The grant covers exactly the one region: a
    /// host-controller driver mints it for itself when it creates the
    /// region, forwards it onto the per-device node it emits, and the
    /// autoloaded class driver inherits it as its sole reach.
    #[must_use]
    pub fn shared(id: u64) -> Self {
        Self::new(HwResourceKind::Shared, id, 1, 0)
    }

    /// A linear scan-out surface at CPU-physical `base`, programmed with
    /// `mode` — the geometry-carrying window a display-class node
    /// publishes so its autoloaded driver can map and drive the surface
    /// (`plans/DISPLAY.md` D7b).
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `mode` is degenerate: a zero
    /// extent, a stride that cannot hold one scanline of `mode.format`
    /// pixels, or a surface length that overflows.
    pub fn framebuffer(
        base: u64,
        mode: &DisplayMode,
        memory: FramebufferMemory,
    ) -> Result<Self, Errno> {
        if mode.width_px == 0 || mode.height_px == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        let min_stride = u64::from(mode.width_px) * u64::from(mode.format.bytes_per_pixel());
        if u64::from(mode.stride_bytes) < min_stride {
            return Err(Errno::LengthOutOfRange);
        }
        let len = u64::from(mode.stride_bytes)
            .checked_mul(u64::from(mode.height_px))
            .ok_or(Errno::LengthOutOfRange)?;
        let xlate = u64::from(mode.width_px) | (u64::from(mode.height_px) << 32);
        let flags = u32::from(mode.format.as_u8())
            | (u32::from(memory.as_u8()) << Self::FRAMEBUFFER_MEMORY_SHIFT);
        Ok(Self::new_xlate(
            HwResourceKind::Framebuffer,
            base,
            len,
            flags,
            xlate,
        ))
    }

    /// Recover the CPU mapping policy carried by a framebuffer resource.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — not a framebuffer resource, or the policy
    ///   discriminant is unknown.
    /// * [`Errno::BadMagic`] — reserved framebuffer flag bits are set.
    pub fn framebuffer_memory(&self) -> Result<FramebufferMemory, Errno> {
        if self.kind() != Some(HwResourceKind::Framebuffer) {
            return Err(Errno::OutOfRange);
        }
        if self.flags & !Self::FRAMEBUFFER_FLAGS_MASK != 0 {
            return Err(Errno::BadMagic);
        }
        #[allow(clippy::cast_possible_truncation)]
        let encoded = (self.flags >> Self::FRAMEBUFFER_MEMORY_SHIFT) as u8;
        FramebufferMemory::from_u8(encoded)
    }

    /// Recover the [`DisplayMode`] a [`Framebuffer`](HwResourceKind::Framebuffer)
    /// resource carries, validating every field — the one decode a
    /// display driver builds its surface from, so a corrupt or hostile
    /// node can never yield a geometry that escapes the window.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — not a framebuffer resource, or an
    ///   unknown pixel format.
    /// * [`Errno::BadMagic`] — reserved format bits set (wire
    ///   corruption).
    /// * [`Errno::LengthOutOfRange`] — a zero extent, a length that is
    ///   not an exact multiple of the height, a stride that overflows
    ///   `u32` or cannot hold one scanline.
    pub fn framebuffer_mode(&self) -> Result<DisplayMode, Errno> {
        if self.kind() != Some(HwResourceKind::Framebuffer) {
            return Err(Errno::OutOfRange);
        }
        if self.flags & !Self::FRAMEBUFFER_FLAGS_MASK != 0 {
            return Err(Errno::BadMagic);
        }
        self.framebuffer_memory()?;
        #[allow(clippy::cast_possible_truncation)]
        let format = DisplayFormat::from_u8((self.flags & Self::FRAMEBUFFER_FORMAT_MASK) as u8)?;
        #[allow(clippy::cast_possible_truncation)]
        let width_px = self.xlate as u32;
        let height_px = u32::try_from(self.xlate >> 32).map_err(|_| Errno::LengthOutOfRange)?;
        if width_px == 0 || height_px == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        if !self.len.is_multiple_of(u64::from(height_px)) {
            return Err(Errno::LengthOutOfRange);
        }
        let stride = self.len / u64::from(height_px);
        let stride_bytes = u32::try_from(stride).map_err(|_| Errno::LengthOutOfRange)?;
        let min_stride = u64::from(width_px) * u64::from(format.bytes_per_pixel());
        if u64::from(stride_bytes) < min_stride {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(DisplayMode {
            width_px,
            height_px,
            stride_bytes,
            format,
        })
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
            Some(HwResourceKind::Mmio | HwResourceKind::Framebuffer) => Some(self.base),
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
    /// Coverage requires identical `flags` between like kinds (and the
    /// blank flags of a plain window when one is re-described, below),
    /// and — for two pairings — allows a cross-kind step. Beyond that the
    /// rule follows each kind's meaning:
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
        // Like kinds must carry identical flags; the cross-kind arms state
        // their own flag rule (a per-kind field is only comparable within
        // its kind).
        if parent_kind == child_kind && self.flags != child.flags {
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
                // discovery): pure CPU-side containment, no wider; both are
                // flagless windows.
                self.flags == child.flags
                    && interval_contains(self.base, self.len, child.base, child.len)
            }
            (HwResourceKind::Mmio, HwResourceKind::Mmio)
            | (HwResourceKind::Port, HwResourceKind::Port)
            | (HwResourceKind::Irq, HwResourceKind::Irq)
            | (HwResourceKind::Endpoint, HwResourceKind::Endpoint)
            | (HwResourceKind::Shared, HwResourceKind::Shared) => {
                // A call-endpoint or shared-region grant is an untranslated
                // `[id, id+len)` range exactly like an IRQ line range: the
                // child must lie wholly within the parent grant.
                interval_contains(self.base, self.len, child.base, child.len)
            }
            (HwResourceKind::Framebuffer, HwResourceKind::Framebuffer) => {
                // A scan-out surface is granted whole, geometry and all:
                // the flags (format) already matched above, and the packed
                // geometry plus the exact window must too — a child cannot
                // reinterpret the parent's surface with a different shape.
                self.xlate == child.xlate && self.base == child.base && self.len == child.len
            }
            (HwResourceKind::Mmio, HwResourceKind::Framebuffer) => {
                // An emitter holding a plain (flagless) window over the
                // surface's memory may publish it as a geometry-carrying
                // scan-out node (the display bring-up path): pure CPU-side
                // containment — the geometry adds description, never reach.
                self.flags == 0 && interval_contains(self.base, self.len, child.base, child.len)
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
/// A node names exactly one detected bus or device function: a stable
/// [`id`], its [`parent`] ([`HW_NODE_ROOT`] for a root), a
/// [`HwDeviceClass`], the [`HwMatchKey`]s the device manager binds
/// against, the [`HwResource`]s it exposes as capability-grant requests,
/// and the bus-local device [`address`] of the physical device the node
/// belongs to. The match-key and resource arrays are fixed-size; the
/// valid prefix of each is given by its count.
///
/// [`id`]: Self::id
/// [`parent`]: Self::parent
/// [`address`]: Self::address
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HwNode {
    id: u32,
    parent: u32,
    address: u32,
    class: u16,
    match_key_count: u8,
    resource_count: u8,
    /// Live recovery health of the fault domain this node *owns*.
    ///
    /// Discovered inventory is otherwise static, but an interior node (a
    /// bus/hub/controller/expander/root complex) that owns a group of
    /// devices beneath it can blip: its driver publishes the domain's
    /// [`FaultDomainState`] here through the `hw_node_health` syscall so the
    /// reactive tree observers (the device manager) learn a hub/controller
    /// reset is *one* fault-domain event rather than N spurious child
    /// removals. A leaf device node has no domain of its own and always
    /// reports [`FaultDomainState::Healthy`]; health is per *owner*, read
    /// from the tree, never hard-coded.
    ///
    /// Stored as the raw [`FaultDomainState::as_u8`] discriminant (mirroring
    /// `class`) so the `#[repr(C)]` layout stays a deterministic single byte
    /// for the generated C view; the [`fault_health`](Self::fault_health)
    /// accessor decodes it fail-closed.
    fault_health: u8,
    match_keys: [HwMatchKey; HW_NODE_MAX_MATCH_KEYS],
    resources: [HwResource; HW_NODE_MAX_RESOURCES],
}

impl HwNode {
    /// Encoded size on the wire: a 17-byte header (16 fixed fields plus the
    /// fault-domain health byte) followed by the full fixed-size match-key
    /// and resource arrays.
    pub const WIRE_LEN: usize = 17
        + HW_NODE_MAX_MATCH_KEYS * HwMatchKey::WIRE_LEN
        + HW_NODE_MAX_RESOURCES * HwResource::WIRE_LEN;

    /// Start a new node with no match keys or resources.
    #[must_use]
    pub fn new(id: u32, parent: u32, class: HwDeviceClass) -> Self {
        Self {
            id,
            parent,
            address: 0,
            class: class.as_u16(),
            match_key_count: 0,
            resource_count: 0,
            fault_health: FaultDomainState::Healthy.as_u8(),
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

    /// Bus-local address of the physical device this node belongs to, or
    /// `0` when the emitter reported none.
    ///
    /// A multi-function device is inventoried as one node per bindable
    /// function (a USB interface, a PCI function), because the function is
    /// the driver-bind and capability-grant unit. The address is how
    /// sibling function nodes of **one** physical device stay attributable
    /// to it: every function node of the same device under the same parent
    /// carries the same non-zero address (a USB host controller reports
    /// the device's xHCI slot id), while two identical devices on one bus
    /// carry distinct addresses. Purely descriptive — driver binding
    /// matches [`HwMatchKey`]s and never reads the address.
    #[must_use]
    pub const fn address(&self) -> u32 {
        self.address
    }

    /// Record the bus-local device address the emitter discovered
    /// (see [`address`](Self::address); `0` means none).
    pub fn set_address(&mut self, address: u32) {
        self.address = address;
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

    /// The live recovery health of the fault domain this node owns
    /// (see [`fault_health`](Self::fault_health) — the struct field docs).
    ///
    /// A leaf device with no domain of its own always reports
    /// [`FaultDomainState::Healthy`]; only an interior owner ever publishes a
    /// [`Recovering`](FaultDomainState::Recovering) or
    /// [`Offline`](FaultDomainState::Offline) reading.
    #[must_use]
    pub const fn fault_health(&self) -> FaultDomainState {
        FaultDomainState::from_u8_fail_closed(self.fault_health)
    }

    /// Record the fault-domain health of the domain this node owns.
    ///
    /// This is the store side of the `hw_node_health` syscall: the kernel
    /// resolves the caller to its *own* matched node before calling this, so
    /// a driver can only ever set the health of the interior node it was
    /// loaded for — never another driver's.
    pub fn set_fault_health(&mut self, health: FaultDomainState) {
        self.fault_health = health.as_u8();
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, self.id);
        put_u32(&mut out, 4, self.parent);
        put_u32(&mut out, 8, self.address);
        put_u16(&mut out, 12, self.class);
        out[14] = self.match_key_count;
        out[15] = self.resource_count;
        out[16] = self.fault_health;
        let mut off = 17;
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
        let class = read_u16(bytes, 12);
        if HwDeviceClass::from_u16(class).is_none() {
            return Err(Errno::OutOfRange);
        }
        let match_key_count = bytes[14];
        let resource_count = bytes[15];
        if usize::from(match_key_count) > HW_NODE_MAX_MATCH_KEYS
            || usize::from(resource_count) > HW_NODE_MAX_RESOURCES
        {
            return Err(Errno::LengthOutOfRange);
        }
        // The health byte is normalised through the fail-closed decoder so a
        // corrupt snapshot can never present a faulted subtree as healthy: an
        // unknown discriminant is stored as Offline.
        let fault_health = FaultDomainState::from_u8_fail_closed(bytes[16]).as_u8();
        let mut match_keys = [HwMatchKey::EMPTY; HW_NODE_MAX_MATCH_KEYS];
        let mut off = 17;
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
            address: read_u32(bytes, 8),
            class,
            match_key_count,
            resource_count,
            fault_health,
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

/// Resolve the **fault-domain owner** of hardware-tree node `node_id`: the id
/// of its nearest strict ancestor that owns a group of devices beneath it — a
/// bus, hub, USB controller, SAS/JBOD expander, or PCIe root complex
/// ([`HwDeviceClass::Bus`]), or the synthetic tree [`HwDeviceClass::Root`] as
/// the domain of last resort for a device attached directly to it.
///
/// This is the pure association the storage fault-isolation layer builds a
/// [`FaultDomain`](crate::blkio::FaultDomain) around: which interior node a
/// block device blips *together with*, read from the discovered tree and never
/// hard-coded, so a USB hub, a SAS expander, and a PCIe root complex are all
/// just interior nodes. It is usable recursively — the owner of an interior
/// node is *its* own nearest such ancestor — so the full chain of nested fault
/// domains up to the root is obtained by re-applying it to each owner in turn.
///
/// Only structural interior classes (`Bus`/`Root`) own a fault domain; a
/// non-owning ancestor (an interrupt controller, a bridge inventoried as
/// something else, or a node whose class discriminant this ABI revision does
/// not know) is skipped and the walk continues upward.
///
/// Fails closed with [`None`] when there is no such owner: `node_id` is absent
/// from `nodes`, the node is itself a root (nothing owns it), or the parent
/// chain is broken or cyclic — a malformed tree is never trusted into an
/// unbounded walk, so the ancestor traversal is bounded by the node count.
#[must_use]
pub fn fault_domain_owner(nodes: &[HwNode], node_id: u32) -> Option<u32> {
    resolve_owner(node_id, nodes.len(), &|id| {
        nodes.iter().find(|node| node.id() == id).map(FaultNode::of)
    })
}

/// The handful of fields the fault-domain walk needs from one node, decoded
/// once from whichever backing the walk runs over — a [`HwNode`] slice
/// (in-memory) or the wire snapshot (`hw_tree_read`). Sharing this view lets
/// the traversal itself be written **once** and reused by both, so the
/// slice-based and snapshot-based fault-domain resolution can never drift
/// apart.
#[derive(Copy, Clone)]
struct FaultNode {
    id: u32,
    parent: u32,
    class: Option<HwDeviceClass>,
    fault_health: FaultDomainState,
}

impl FaultNode {
    fn of(node: &HwNode) -> Self {
        Self {
            id: node.id(),
            parent: node.parent(),
            class: node.class(),
            fault_health: node.fault_health(),
        }
    }

    const fn is_root(&self) -> bool {
        self.parent == HW_NODE_ROOT
    }
}

/// Resolve the nearest fault-domain owner of `node_id`, looking each node up
/// by id through `lookup` — the single traversal behind both
/// [`fault_domain_owner`] (a slice lookup) and the wire-snapshot ancestor
/// fold ([`ancestor_imposed_status_from_snapshot`], a byte lookup).
///
/// It walks strictly upward to the nearest [`HwDeviceClass::Bus`] /
/// [`HwDeviceClass::Root`] owner exactly as [`fault_domain_owner`] documents,
/// and is bounded by `bound` (the node count) so a broken or cyclic chain
/// ends the walk rather than spinning (fail closed, `None`).
fn resolve_owner<F>(node_id: u32, bound: usize, lookup: &F) -> Option<u32>
where
    F: Fn(u32) -> Option<FaultNode>,
{
    let mut current = lookup(node_id)?;
    for _ in 0..bound {
        if current.is_root() {
            return None;
        }
        let parent = lookup(current.parent)?;
        if matches!(parent.class, Some(HwDeviceClass::Bus | HwDeviceClass::Root)) {
            return Some(parent.id);
        }
        current = parent;
    }
    None
}

/// Fold the imposed child status of every interior fault-domain ancestor of
/// `node_id`, nearest first, looking nodes up through `lookup` — the single
/// definition behind both [`ancestor_imposed_status`] (a slice lookup) and
/// [`ancestor_imposed_status_from_snapshot`] (a byte lookup).
///
/// Each owner in the chain contributes its published
/// [`FaultDomainState::imposed_child_status`], combined through
/// [`BlkStatus::combine`]'s severity total order, so a deep failing ancestor
/// is never masked by a shallower healthy one. Bounded by `bound` and
/// fail-closed exactly as [`resolve_owner`].
fn fold_ancestor_status<F>(node_id: u32, bound: usize, lookup: &F) -> BlkStatus
where
    F: Fn(u32) -> Option<FaultNode>,
{
    let mut status = BlkStatus::Ok;
    let mut current = node_id;
    for _ in 0..bound {
        let Some(owner_id) = resolve_owner(current, bound, lookup) else {
            break;
        };
        if let Some(owner) = lookup(owner_id) {
            if let Some(imposed) = owner.fault_health.imposed_child_status() {
                status = status.combine(imposed);
            }
        }
        current = owner_id;
    }
    status
}

/// The ordered chain of nested fault-domain owners of hardware-tree node
/// `node_id`, **nearest first**, up to and including the synthetic root — the
/// full set of interior nodes a block device blips *together with*.
///
/// Where [`fault_domain_owner`] resolves only the *nearest* owner, a serving
/// driver needs the whole nested chain (leaf → hub → controller → root) to
/// build one [`FaultDomain`](crate::blkio::FaultDomain) per interior node and
/// fold them with the leaf's own outcome through
/// [`effective_child_status`](crate::blkio::effective_child_status). This is
/// that chain, computed by re-applying `fault_domain_owner` to each owner in
/// turn — the single definition of the recursion, so a driver never re-derives
/// the walk itself.
///
/// It is lazy and allocation-free: it yields owner ids one at a time and holds
/// only a borrow of `nodes`, so the chain has no fixed-depth ceiling. It fails
/// closed exactly as [`fault_domain_owner`] does — an absent `node_id`, a root
/// (nothing owns it), or a broken parent chain ends the walk with no further
/// owner — and it is cycle-safe: a malformed tree can never drive an unbounded
/// walk, because the iterator is bounded to at most `nodes.len()` steps (a
/// chain of strict ancestors visits distinct nodes, so a valid chain is never
/// truncated, and a cyclic one simply stops rather than spins).
#[must_use]
pub fn fault_domain_chain(nodes: &[HwNode], node_id: u32) -> FaultDomainChain<'_> {
    FaultDomainChain {
        nodes,
        current: Some(node_id),
        remaining: nodes.len(),
    }
}

/// Lazy iterator over the nested fault-domain owners of a hardware-tree node,
/// nearest first. Created by [`fault_domain_chain`]; see it for the semantics
/// and the fail-closed / cycle-safe guarantees.
#[derive(Clone, Debug)]
pub struct FaultDomainChain<'a> {
    nodes: &'a [HwNode],
    /// The node whose owner the next [`Iterator::next`] resolves. `None` once
    /// the walk has reached the top (a root has no owner) or failed closed, so
    /// the iterator is fused.
    current: Option<u32>,
    /// Remaining steps, initialised to the node count. A strict-ancestor chain
    /// visits distinct nodes, so it is at most `nodes.len()` long; the bound
    /// makes a broken or cyclic tree end the walk rather than spin instead of
    /// being trusted into an unbounded traversal.
    remaining: usize,
}

impl Iterator for FaultDomainChain<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let current = self.current?;
        if self.remaining == 0 {
            // A well-formed chain has terminated before now; reaching the bound
            // means a cyclic tree. Fail closed rather than yield forever.
            self.current = None;
            return None;
        }
        self.remaining -= 1;
        // Fail closed and fuse the iterator once an owner cannot be resolved.
        let owner = fault_domain_owner(self.nodes, current);
        self.current = owner;
        owner
    }
}

/// The [`BlkStatus`] the *published* health of hardware-tree node `node_id`'s
/// interior fault-domain ancestors imposes on that leaf device's block-service
/// completions, at the moment `nodes` was snapshotted.
///
/// This is the leaf-driver counterpart of
/// [`effective_child_status`](crate::blkio::effective_child_status): where that
/// folds fault [`domains`](crate::blkio::FaultDomain) a serving driver owns and
/// clocks itself, a leaf block device's interior ancestors — the USB
/// controller, the hub, the PCIe root complex — live in *other* driver
/// processes, which publish their own recovery health onto the shared tree
/// through [`crate::SyscallNumber::HW_NODE_HEALTH`] (recorded on
/// [`HwNode::fault_health`]). A leaf driver reads that published health for its
/// whole ancestor [`chain`](fault_domain_chain) and folds each owner's imposed
/// [`FaultDomainState::imposed_child_status`] through [`BlkStatus::combine`], so
/// one controller/hub reset is attributed to the *fault domain* — the leaf's
/// completions carry the reissuable [`BlkStatus::Reset`] (or, once an ancestor
/// has failed closed, [`BlkStatus::Offline`]) — rather than looking like an
/// independent failure of the disk itself. A published state is already
/// resolved by its owner (the owner ran its own grace window before emitting),
/// so no clock is read here; the fold uses the one shared owner-health rule
/// [`FaultDomainState::imposed_child_status`] defines, exactly as an owned
/// [`FaultDomain`](crate::blkio::FaultDomain) does.
///
/// Returns [`BlkStatus::Ok`] when every ancestor is
/// [`Healthy`](FaultDomainState::Healthy) (or the chain is empty / fails
/// closed): a healthy tree imposes nothing and the leaf answers on its own
/// per-device health. It inherits [`fault_domain_chain`]'s fail-closed,
/// cycle-safe, allocation-free walk — an absent, rootless, or broken node
/// simply yields no imposing ancestor. The `combine` fold is over
/// [`BlkStatus`]'s severity total order, so a deep failing ancestor is never
/// masked by a shallower healthy one.
#[must_use]
pub fn ancestor_imposed_status(nodes: &[HwNode], node_id: u32) -> BlkStatus {
    fold_ancestor_status(node_id, nodes.len(), &|id| {
        nodes.iter().find(|node| node.id() == id).map(FaultNode::of)
    })
}

/// [`ancestor_imposed_status`] over the **wire snapshot** a driver reads with
/// `hw_tree_read`, rather than an in-memory [`HwNode`] slice.
///
/// A leaf block driver does not hold the tree as a slice — it reads the
/// kernel's wire snapshot (a [`HwTreeHeader`] followed by
/// [`HwTreeHeader::node_count`] records of [`HwNode::WIRE_LEN`] bytes) into a
/// buffer. This computes the same interior-ancestor fold directly over those
/// bytes, so the driver need not materialise the whole tree (no allocation on
/// its recovery path). It shares the one traversal `fold_ancestor_status`
/// defines — only the per-id lookup differs (a byte scan here, a slice scan in
/// [`ancestor_imposed_status`]) — so the two can never diverge.
///
/// A malformed or truncated snapshot (a short header, a length whose byte span
/// overflows, or a buffer shorter than the records it promises) imposes
/// nothing and returns [`BlkStatus::Ok`]: the driver **degrades safe** to
/// answering on the device's own health rather than fabricating a fault from a
/// snapshot it cannot trust. This is the read side of the cross-process
/// fault-domain signal (`plans/FIX-IO.md` IO4): a leaf attributes a
/// controller/hub blip to the fault domain instead of to the disk.
#[must_use]
pub fn ancestor_imposed_status_from_snapshot(blob: &[u8], node_id: u32) -> BlkStatus {
    // A malformed or truncated snapshot is never partially interpreted: it
    // imposes nothing, so the driver degrades safe to answering on the
    // device's own health rather than fabricating a fault from bytes it
    // cannot trust.
    let Some(snapshot) = snapshot_nodes(blob) else {
        return BlkStatus::Ok;
    };
    let count = snapshot.len();
    let lookup = |id: u32| -> Option<FaultNode> {
        snapshot
            .clone()
            .find(|node| node.id() == id)
            .map(|node| FaultNode::of(&node))
    };
    fold_ancestor_status(node_id, count, &lookup)
}

/// The nodes of a kernel hardware-tree snapshot, decoded lazily in wire order.
///
/// Built by [`snapshot_nodes`]; holds only a borrow of the snapshot bytes, so
/// walking a tree of any size costs no allocation and imposes no node ceiling.
/// A record whose bytes do not decode is **skipped**, never half-trusted, so a
/// corrupt record can hide a node but can never invent one.
#[derive(Clone, Debug)]
pub struct HwTreeNodes<'a> {
    /// The record region alone: the header is consumed by [`snapshot_nodes`],
    /// which has already validated that this holds exactly `remaining` whole
    /// records.
    records: &'a [u8],
    remaining: usize,
}

impl HwTreeNodes<'_> {
    /// How many records the snapshot promised.
    ///
    /// A count, not a yield guarantee: an undecodable record is skipped, so the
    /// iterator can yield fewer nodes than this.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.remaining
    }

    /// Whether the snapshot promised no records at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

impl Iterator for HwTreeNodes<'_> {
    type Item = HwNode;

    fn next(&mut self) -> Option<HwNode> {
        while self.remaining > 0 {
            // The checked split keeps the walk total: the constructor sized
            // `records` to exactly `remaining` whole records, so this cannot
            // come up short, and taking the checked form means a future change
            // that broke that pairing would end the walk rather than panic in
            // a driver's request path.
            let Some((record, rest)) = self.records.split_at_checked(HwNode::WIRE_LEN) else {
                self.remaining = 0;
                return None;
            };
            self.records = rest;
            self.remaining -= 1;
            if let Ok(node) = HwNode::from_bytes(record) {
                return Some(node);
            }
        }
        None
    }
}

/// Read the **wire snapshot** a task obtains from `hw_tree_read` — a
/// [`HwTreeHeader`] followed by [`HwTreeHeader::node_count`] records of
/// [`HwNode::WIRE_LEN`] bytes — as an iterator over its nodes.
///
/// This is the one definition of how the kernel's snapshot is walked, so a
/// consumer never re-derives the header validation or the record stride. It
/// fails closed to [`None`] for a snapshot whose promised extent cannot be
/// trusted — a short or malformed header, a node count that does not fit the
/// host, or a byte span that overflows or exceeds `blob` — rather than
/// interpreting a truncated tree, because a partially-read tree would silently
/// omit nodes a security decision may depend on.
#[must_use]
pub fn snapshot_nodes(blob: &[u8]) -> Option<HwTreeNodes<'_>> {
    let header = HwTreeHeader::from_bytes(blob).ok()?;
    let count = usize::try_from(header.node_count()).ok()?;
    let span = count
        .checked_mul(HwNode::WIRE_LEN)?
        .checked_add(HwTreeHeader::WIRE_LEN)?;
    if blob.len() < span {
        return None;
    }
    Some(HwTreeNodes {
        records: &blob[HwTreeHeader::WIRE_LEN..span],
        remaining: count,
    })
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
        assert_eq!(
            HwResourceKind::Endpoint.required_capability(),
            CapabilityId::IPC_ENDPOINT
        );
        assert_eq!(
            HwResourceKind::Shared.required_capability(),
            CapabilityId::SHM
        );
        assert_eq!(
            HwResourceKind::Framebuffer.required_capability(),
            CapabilityId::MMIO_MAP
        );
    }

    #[test]
    fn framebuffer_resource_round_trips_its_mode() {
        let mode = DisplayMode {
            width_px: 1280,
            height_px: 720,
            stride_bytes: 5120,
            format: DisplayFormat::Bgra8888,
        };
        let fb = HwResource::framebuffer(0x4000_0000, &mode, FramebufferMemory::WriteCombine)
            .expect("valid mode");
        assert_eq!(fb.kind(), Some(HwResourceKind::Framebuffer));
        assert_eq!(fb.base(), 0x4000_0000);
        assert_eq!(fb.length(), 5120 * 720);
        assert_eq!(fb.required_capability(), Ok(CapabilityId::MMIO_MAP));
        assert_eq!(fb.register_window_base(), Some(0x4000_0000));
        assert_eq!(fb.framebuffer_mode(), Ok(mode));
        assert_eq!(HwResource::from_bytes(&fb.to_le_bytes()).unwrap(), fb);
        assert_eq!(
            HwResourceKind::from_u16(7),
            Some(HwResourceKind::Framebuffer)
        );
    }

    #[test]
    fn framebuffer_resource_round_trips_its_memory_policy() {
        let mode = DisplayMode {
            width_px: 1280,
            height_px: 720,
            stride_bytes: 5120,
            format: DisplayFormat::Bgra8888,
        };
        for memory in [
            FramebufferMemory::WriteBack,
            FramebufferMemory::WriteCombine,
        ] {
            let fb = HwResource::framebuffer(0x4000_0000, &mode, memory).expect("valid mode");
            assert_eq!(fb.framebuffer_memory(), Ok(memory));
            let decoded = HwResource::from_bytes(&fb.to_le_bytes()).expect("resource decodes");
            assert_eq!(decoded.framebuffer_memory(), Ok(memory));
        }
    }

    #[test]
    fn framebuffer_constructor_refuses_degenerate_modes() {
        let mode = |w: u32, h: u32, stride: u32| DisplayMode {
            width_px: w,
            height_px: h,
            stride_bytes: stride,
            format: DisplayFormat::Rgba8888,
        };
        assert_eq!(
            HwResource::framebuffer(0, &mode(0, 720, 5120), FramebufferMemory::WriteCombine),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            HwResource::framebuffer(0, &mode(1280, 0, 5120), FramebufferMemory::WriteCombine),
            Err(Errno::LengthOutOfRange)
        );
        // A stride that cannot hold one 1280-px scanline (needs 5120).
        assert_eq!(
            HwResource::framebuffer(0, &mode(1280, 720, 5119), FramebufferMemory::WriteCombine,),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn framebuffer_mode_decode_fails_closed_on_hostile_fields() {
        let mode = DisplayMode {
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        };
        let good = HwResource::framebuffer(0x1000, &mode, FramebufferMemory::WriteCombine)
            .expect("valid mode");
        let valid_flags = u32::from(DisplayFormat::Bgra8888.as_u8())
            | (u32::from(FramebufferMemory::WriteCombine.as_u8())
                << HwResource::FRAMEBUFFER_MEMORY_SHIFT);

        // Not a framebuffer resource at all.
        assert_eq!(
            HwResource::mmio(0x1000, 32).framebuffer_mode(),
            Err(Errno::OutOfRange)
        );
        // Reserved flag bits / unknown format byte.
        let dirty_flags = HwResource::new(
            HwResourceKind::Framebuffer,
            0x1000,
            32,
            valid_flags | 0x1_0000,
        );
        assert_eq!(dirty_flags.framebuffer_mode(), Err(Errno::BadMagic));
        let bad_format = HwResource::new(
            HwResourceKind::Framebuffer,
            0x1000,
            32,
            (valid_flags & !HwResource::FRAMEBUFFER_FORMAT_MASK) | 9,
        );
        assert_eq!(bad_format.framebuffer_mode(), Err(Errno::OutOfRange));
        // Zero geometry, a non-exact length, an undersized stride.
        let zero_geo =
            HwResource::new_xlate(HwResourceKind::Framebuffer, 0x1000, 32, valid_flags, 0);
        assert_eq!(zero_geo.framebuffer_mode(), Err(Errno::LengthOutOfRange));
        let ragged = HwResource::new_xlate(
            HwResourceKind::Framebuffer,
            0x1000,
            33,
            valid_flags,
            4 | (2u64 << 32),
        );
        assert_eq!(ragged.framebuffer_mode(), Err(Errno::LengthOutOfRange));
        let thin = HwResource::new_xlate(
            HwResourceKind::Framebuffer,
            0x1000,
            24, // stride 12 < 4 px × 4 bytes
            valid_flags,
            4 | (2u64 << 32),
        );
        assert_eq!(thin.framebuffer_mode(), Err(Errno::LengthOutOfRange));
        // The well-formed resource still decodes.
        assert_eq!(good.framebuffer_mode(), Ok(mode));
    }

    #[test]
    fn framebuffer_coverage_is_exact_or_from_a_plain_window() {
        let mode = DisplayMode {
            width_px: 4,
            height_px: 2,
            stride_bytes: 16,
            format: DisplayFormat::Bgra8888,
        };
        let fb = HwResource::framebuffer(0x1000, &mode, FramebufferMemory::WriteCombine)
            .expect("valid mode");
        // Identical surface: covered.
        assert!(fb.covers(
            &HwResource::framebuffer(0x1000, &mode, FramebufferMemory::WriteCombine).unwrap()
        ));
        // A different window or geometry is not.
        assert!(!fb.covers(
            &HwResource::framebuffer(0x2000, &mode, FramebufferMemory::WriteCombine).unwrap()
        ));
        let other = DisplayMode {
            width_px: 2,
            height_px: 4,
            stride_bytes: 8,
            format: DisplayFormat::Bgra8888,
        };
        assert!(!fb.covers(
            &HwResource::framebuffer(0x1000, &other, FramebufferMemory::WriteCombine).unwrap()
        ));
        assert!(!fb.covers(
            &HwResource::framebuffer(0x1000, &mode, FramebufferMemory::WriteBack).unwrap()
        ));
        // A plain window over the surface memory covers the described
        // surface; a disjoint or short window does not, and the
        // geometry-carrying grant never covers a plain window.
        assert!(HwResource::mmio(0x1000, 32).covers(&fb));
        assert!(HwResource::mmio(0x0, 0x10000).covers(&fb));
        assert!(!HwResource::mmio(0x1000, 16).covers(&fb));
        assert!(!HwResource::mmio(0x2000, 32).covers(&fb));
        assert!(!fb.covers(&HwResource::mmio(0x1000, 32)));
    }

    #[test]
    fn endpoint_resource_round_trips_and_covers_only_itself() {
        let ep = HwResource::endpoint(0xC0FF_EE01);
        assert_eq!(ep.kind(), Some(HwResourceKind::Endpoint));
        assert_eq!(ep.base(), 0xC0FF_EE01);
        assert_eq!(ep.length(), 1);
        assert_eq!(ep.required_capability(), Ok(CapabilityId::IPC_ENDPOINT));
        assert_eq!(HwResource::from_bytes(&ep.to_le_bytes()).unwrap(), ep);
        // An endpoint grant covers exactly its own id and no neighbour, and
        // never a different resource kind.
        assert!(ep.covers(&HwResource::endpoint(0xC0FF_EE01)));
        assert!(!ep.covers(&HwResource::endpoint(0xC0FF_EE02)));
        assert!(!ep.covers(&HwResource::endpoint(0xC0FF_EE00)));
        assert!(!ep.covers(&HwResource::irq(0xC0FF_EE01, 1)));
        // The endpoint kind decodes from its wire discriminant.
        assert_eq!(HwResourceKind::from_u16(5), Some(HwResourceKind::Endpoint));
    }

    #[test]
    fn shared_resource_round_trips_and_covers_only_itself() {
        let region = HwResource::shared(0x5EED_0001);
        assert_eq!(region.kind(), Some(HwResourceKind::Shared));
        assert_eq!(region.base(), 0x5EED_0001);
        assert_eq!(region.length(), 1);
        assert_eq!(region.required_capability(), Ok(CapabilityId::SHM));
        assert_eq!(
            HwResource::from_bytes(&region.to_le_bytes()).unwrap(),
            region
        );
        // A shared-region grant covers exactly its own id and no neighbour,
        // and never a different resource kind.
        assert!(region.covers(&HwResource::shared(0x5EED_0001)));
        assert!(!region.covers(&HwResource::shared(0x5EED_0002)));
        assert!(!region.covers(&HwResource::shared(0x5EED_0000)));
        assert!(!region.covers(&HwResource::endpoint(0x5EED_0001)));
        assert!(!region.covers(&HwResource::irq(0x5EED_0001, 1)));
        // The shared kind decodes from its wire discriminant.
        assert_eq!(HwResourceKind::from_u16(6), Some(HwResourceKind::Shared));
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
        node.set_address(5);
        node
    }

    #[test]
    fn node_round_trips() {
        let node = sample_node();
        assert_eq!(node.id(), 7);
        assert_eq!(node.parent(), 0);
        assert!(!node.is_root());
        assert_eq!(node.address(), 5);
        assert_eq!(node.class(), Some(HwDeviceClass::Network));
        assert_eq!(node.match_keys().len(), 2);
        assert_eq!(node.resources().len(), 2);

        let bytes = node.to_le_bytes();
        assert_eq!(bytes.len(), HwNode::WIRE_LEN);
        let back = HwNode::from_bytes(&bytes).expect("decode");
        assert_eq!(back, node);
        assert_eq!(back.address(), 5);
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
        assert_eq!(root.address(), 0, "a fresh node reports no device address");
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
        put_u16(&mut bytes, 12, 11); // unknown device class
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::OutOfRange));

        let mut bytes = sample_node().to_le_bytes();
        bytes[14] = u8::try_from(HW_NODE_MAX_MATCH_KEYS + 1).unwrap();
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::LengthOutOfRange));

        let mut bytes = sample_node().to_le_bytes();
        bytes[15] = u8::try_from(HW_NODE_MAX_RESOURCES + 1).unwrap();
        assert_eq!(HwNode::from_bytes(&bytes), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn wire_lengths_are_frozen() {
        // Pinned so an accidental layout change is caught.
        assert_eq!(HwMatchKey::WIRE_LEN, 76);
        assert_eq!(HwResource::WIRE_LEN, 32);
        assert_eq!(GrantedResource::WIRE_LEN, 40);
        // 17-byte header (the 16 fixed fields plus the fault-domain health
        // byte) followed by the fixed match-key and resource arrays.
        assert_eq!(HwNode::WIRE_LEN, 577);
        assert_eq!(HwTreeHeader::WIRE_LEN, 16);
    }

    #[test]
    fn a_fresh_node_owns_a_healthy_fault_domain() {
        // A node built by `new` reports Healthy: a leaf device has no domain
        // of its own, and an interior owner starts up.
        let node = HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Bus);
        assert_eq!(node.fault_health(), FaultDomainState::Healthy);
    }

    #[test]
    fn fault_health_round_trips_through_the_wire() {
        for health in [
            FaultDomainState::Healthy,
            FaultDomainState::Recovering,
            FaultDomainState::Offline,
        ] {
            let mut node = sample_node();
            node.set_fault_health(health);
            assert_eq!(node.fault_health(), health);
            let bytes = node.to_le_bytes();
            // The health rides in the single header byte at offset 16.
            assert_eq!(bytes[16], health.as_u8());
            let decoded = HwNode::from_bytes(&bytes).expect("decodes");
            assert_eq!(decoded.fault_health(), health);
            assert_eq!(decoded, node);
        }
    }

    #[test]
    fn an_unknown_health_byte_decodes_fail_closed_to_offline() {
        // A corrupt snapshot must never present a faulted subtree as healthy:
        // an out-of-range health discriminant reads as Offline.
        let mut bytes = sample_node().to_le_bytes();
        bytes[16] = 0xFF;
        let decoded = HwNode::from_bytes(&bytes).expect("decodes");
        assert_eq!(decoded.fault_health(), FaultDomainState::Offline);
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

    /// A small USB-shaped tree:
    /// root(0) → xHCI controller bus(1) → hub bus(2) → disk(3);
    /// a disk(4) directly on the controller; a disk(5) directly on the root.
    fn usb_shaped_tree() -> [HwNode; 6] {
        [
            HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(1, 0, HwDeviceClass::Bus),
            HwNode::new(2, 1, HwDeviceClass::Bus),
            HwNode::new(3, 2, HwDeviceClass::Storage),
            HwNode::new(4, 1, HwDeviceClass::Storage),
            HwNode::new(5, 0, HwDeviceClass::Storage),
        ]
    }

    #[test]
    fn fault_domain_owner_is_the_nearest_bus_ancestor() {
        let nodes = usb_shaped_tree();
        // The disk on the hub blips with the hub, not the controller above it.
        assert_eq!(fault_domain_owner(&nodes, 3), Some(2));
        // The disk directly on the controller blips with the controller.
        assert_eq!(fault_domain_owner(&nodes, 4), Some(1));
    }

    #[test]
    fn fault_domain_owner_falls_back_to_the_root_for_a_directly_attached_device() {
        let nodes = usb_shaped_tree();
        assert_eq!(fault_domain_owner(&nodes, 5), Some(0));
    }

    #[test]
    fn fault_domain_owner_nests_through_interior_nodes_up_to_the_root() {
        let nodes = usb_shaped_tree();
        // Re-applying the resolution walks the whole chain of nested domains.
        assert_eq!(fault_domain_owner(&nodes, 2), Some(1));
        assert_eq!(fault_domain_owner(&nodes, 1), Some(0));
        // The root itself has no owner above it.
        assert_eq!(fault_domain_owner(&nodes, 0), None);
    }

    #[test]
    fn fault_domain_owner_skips_a_non_owning_ancestor() {
        // A disk(11) under a node(10) that is neither a bus nor the root: the
        // owner is the bus(1) above that node, not the non-owning node.
        let nodes = [
            HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(1, 0, HwDeviceClass::Bus),
            HwNode::new(10, 1, HwDeviceClass::Other),
            HwNode::new(11, 10, HwDeviceClass::Storage),
        ];
        assert_eq!(fault_domain_owner(&nodes, 11), Some(1));
    }

    #[test]
    fn fault_domain_owner_fails_closed_on_an_absent_or_broken_or_cyclic_tree() {
        let nodes = usb_shaped_tree();
        // A node id that is not in the tree resolves to nothing.
        assert_eq!(fault_domain_owner(&nodes, 999), None);

        // A device whose parent id is dangling fails closed rather than
        // fabricating an owner.
        let broken = [HwNode::new(20, 21, HwDeviceClass::Storage)];
        assert_eq!(fault_domain_owner(&broken, 20), None);

        // A cyclic chain of non-owning nodes is bounded, not walked forever.
        let cyclic = [
            HwNode::new(30, 31, HwDeviceClass::Other),
            HwNode::new(31, 30, HwDeviceClass::Other),
        ];
        assert_eq!(fault_domain_owner(&cyclic, 30), None);
    }

    #[test]
    fn fault_domain_chain_yields_every_nested_owner_nearest_first() {
        let nodes = usb_shaped_tree();
        // The disk on the hub blips with the hub, then the controller, then
        // the root — the full chain of nested fault domains, nearest first.
        let mut chain = fault_domain_chain(&nodes, 3);
        assert_eq!(chain.next(), Some(2));
        assert_eq!(chain.next(), Some(1));
        assert_eq!(chain.next(), Some(0));
        assert_eq!(chain.next(), None);
        // Fused: exhausted stays exhausted.
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn fault_domain_chain_from_an_interior_node_starts_above_it() {
        let nodes = usb_shaped_tree();
        // A device directly on the controller has the controller then the root.
        let mut chain = fault_domain_chain(&nodes, 4);
        assert_eq!(chain.next(), Some(1));
        assert_eq!(chain.next(), Some(0));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn fault_domain_chain_falls_back_to_just_the_root() {
        let nodes = usb_shaped_tree();
        // A device directly on the root has only the root as its domain.
        let mut chain = fault_domain_chain(&nodes, 5);
        assert_eq!(chain.next(), Some(0));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn fault_domain_chain_of_the_root_itself_is_empty() {
        let nodes = usb_shaped_tree();
        // The root owns no domain above it, so its chain is empty.
        assert_eq!(fault_domain_chain(&nodes, 0).next(), None);
    }

    #[test]
    fn fault_domain_chain_skips_a_non_owning_ancestor() {
        // The disk(11) under a non-owning node(10) resolves to the bus(1) then
        // the root(0), skipping the intervening non-owning node.
        let nodes = [
            HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(1, 0, HwDeviceClass::Bus),
            HwNode::new(10, 1, HwDeviceClass::Other),
            HwNode::new(11, 10, HwDeviceClass::Storage),
        ];
        let mut chain = fault_domain_chain(&nodes, 11);
        assert_eq!(chain.next(), Some(1));
        assert_eq!(chain.next(), Some(0));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn fault_domain_chain_fails_closed_on_absent_or_broken_trees() {
        let nodes = usb_shaped_tree();
        // A node not in the tree yields nothing.
        assert_eq!(fault_domain_chain(&nodes, 999).next(), None);

        // A device whose parent id dangles yields nothing rather than
        // fabricating an owner.
        let broken = [HwNode::new(20, 21, HwDeviceClass::Storage)];
        assert_eq!(fault_domain_chain(&broken, 20).next(), None);
    }

    #[test]
    fn fault_domain_chain_is_bounded_on_a_cyclic_bus_tree() {
        // Two buses that parent each other would drive a naive re-application
        // forever (each resolves the other as its owner on the first hop). The
        // chain is bounded by the node count, so it terminates.
        let cyclic = [
            HwNode::new(40, 41, HwDeviceClass::Bus),
            HwNode::new(41, 40, HwDeviceClass::Bus),
            HwNode::new(42, 40, HwDeviceClass::Storage),
        ];
        let mut yielded = 0usize;
        for owner in fault_domain_chain(&cyclic, 42) {
            assert!(owner == 40 || owner == 41);
            yielded += 1;
            assert!(yielded <= cyclic.len(), "the chain must be bounded");
        }
        // It made at least one hop and then stopped inside the bound.
        assert!((1..=cyclic.len()).contains(&yielded));
    }

    #[test]
    fn ancestor_imposed_status_is_ok_when_the_whole_chain_is_healthy() {
        let nodes = usb_shaped_tree();
        // Every ancestor (hub, controller, root) is Healthy, so nothing is
        // imposed and the leaf answers on its own per-device health.
        assert_eq!(ancestor_imposed_status(&nodes, 3), BlkStatus::Ok);
        // The root itself has no ancestors: also nothing imposed.
        assert_eq!(ancestor_imposed_status(&nodes, 0), BlkStatus::Ok);
    }

    #[test]
    fn a_recovering_ancestor_makes_a_leaf_reissuable() {
        let mut nodes = usb_shaped_tree();
        // The USB controller (node 1) is mid-reset: the disk on the hub below
        // it (node 3) is held reissuable under the controller's window, so one
        // controller blip is attributed to the fault domain, not the disk.
        nodes[1].set_fault_health(FaultDomainState::Recovering);
        assert_eq!(ancestor_imposed_status(&nodes, 3), BlkStatus::Reset);
        // The disk directly on the controller (node 4) is affected too.
        assert_eq!(ancestor_imposed_status(&nodes, 4), BlkStatus::Reset);
        // A disk on a different branch (directly on the root, node 5) is not.
        assert_eq!(ancestor_imposed_status(&nodes, 5), BlkStatus::Ok);
    }

    #[test]
    fn an_offline_ancestor_fails_a_leaf_closed() {
        let mut nodes = usb_shaped_tree();
        // The hub (node 2) failed closed (its grace window elapsed): the disk
        // beneath it (node 3) is failed closed to Offline.
        nodes[2].set_fault_health(FaultDomainState::Offline);
        assert_eq!(ancestor_imposed_status(&nodes, 3), BlkStatus::Offline);
        // The disk directly on the controller (node 4) is on a healthy branch.
        assert_eq!(ancestor_imposed_status(&nodes, 4), BlkStatus::Ok);
    }

    #[test]
    fn a_deep_failing_ancestor_is_never_masked_by_a_shallow_healthy_one() {
        let mut nodes = usb_shaped_tree();
        // The nearest ancestor (the hub, node 2) is Healthy, but the
        // controller above it (node 1) has failed closed: the leaf (node 3)
        // must still see Offline — the shallow healthy hub cannot mask the
        // deep failing controller (the severity total order wins).
        nodes[1].set_fault_health(FaultDomainState::Offline);
        assert_eq!(ancestor_imposed_status(&nodes, 3), BlkStatus::Offline);
    }

    #[test]
    fn ancestor_imposed_status_fails_closed_to_ok_on_absent_or_broken_trees() {
        let nodes = usb_shaped_tree();
        // A node not in the tree has no resolvable chain: nothing imposed.
        assert_eq!(ancestor_imposed_status(&nodes, 999), BlkStatus::Ok);
        // A dangling parent yields no owner, so no ancestor imposes anything.
        let broken = [HwNode::new(20, 21, HwDeviceClass::Storage)];
        assert_eq!(ancestor_imposed_status(&broken, 20), BlkStatus::Ok);
    }

    /// Encode `[HwTreeHeader][HwNode; n]` into `out` exactly as the kernel
    /// source does, returning the encoded length. Alloc-free so it runs in
    /// the `no_std` `lib/abi` test build.
    fn encode_snapshot(out: &mut [u8], generation: u64, nodes: &[HwNode]) -> usize {
        let header = HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes();
        out[..HwTreeHeader::WIRE_LEN].copy_from_slice(&header);
        let mut off = HwTreeHeader::WIRE_LEN;
        for node in nodes {
            out[off..off + HwNode::WIRE_LEN].copy_from_slice(&node.to_le_bytes());
            off += HwNode::WIRE_LEN;
        }
        off
    }

    #[test]
    fn the_snapshot_ancestor_fold_matches_the_slice_fold_for_every_health() {
        // The byte-backed and slice-backed folds share one traversal, so they
        // must agree node-for-node under every ancestor health arrangement.
        const CAP: usize = HwTreeHeader::WIRE_LEN + 6 * HwNode::WIRE_LEN;
        for (recovering, offline) in [
            (None, None),         // whole tree healthy
            (Some(1usize), None), // controller recovering
            (Some(2), None),      // hub recovering
            (None, Some(2usize)), // hub offline
            (None, Some(1)),      // controller offline (deep)
            (Some(2), Some(1)),   // hub recovering under an offline controller
        ] {
            let mut nodes = usb_shaped_tree();
            if let Some(i) = recovering {
                nodes[i].set_fault_health(FaultDomainState::Recovering);
            }
            if let Some(i) = offline {
                nodes[i].set_fault_health(FaultDomainState::Offline);
            }
            let mut blob = [0u8; CAP];
            let len = encode_snapshot(&mut blob, 7, &nodes);
            for leaf in [3u32, 4, 5, 0, 999] {
                assert_eq!(
                    ancestor_imposed_status_from_snapshot(&blob[..len], leaf),
                    ancestor_imposed_status(&nodes, leaf),
                    "snapshot and slice folds must agree for leaf {leaf}",
                );
            }
        }
    }

    #[test]
    fn a_snapshot_reads_back_as_the_nodes_it_was_encoded_from() {
        // The one definition of how the kernel's wire snapshot is walked: every
        // record decodes back to the node it was encoded from, in wire order,
        // and the promised count is reported without decoding anything.
        let nodes = usb_shaped_tree();
        let mut blob = [0u8; HwTreeHeader::WIRE_LEN + 6 * HwNode::WIRE_LEN];
        let len = encode_snapshot(&mut blob, 11, &nodes);
        let snapshot = snapshot_nodes(&blob[..len]).expect("a well-formed snapshot reads");
        assert_eq!(snapshot.len(), nodes.len());
        assert!(!snapshot.is_empty());
        let mut seen = 0usize;
        for (read, expected) in snapshot.zip(nodes.iter()) {
            assert_eq!(read.id(), expected.id());
            assert_eq!(read.parent(), expected.parent());
            assert_eq!(read.class(), expected.class());
            assert_eq!(read.resources(), expected.resources());
            assert_eq!(read.match_keys(), expected.match_keys());
            seen += 1;
        }
        assert_eq!(seen, nodes.len(), "every promised record is yielded");
    }

    #[test]
    fn a_snapshot_whose_extent_cannot_be_trusted_reads_as_nothing() {
        // A partially-read tree would silently omit nodes a security decision
        // may depend on, so a snapshot that does not hold what its header
        // promises is refused outright rather than walked as far as it goes.
        assert!(snapshot_nodes(&[0u8; 4]).is_none(), "a short header");
        let nodes = usb_shaped_tree();
        let mut blob = [0u8; HwTreeHeader::WIRE_LEN + 6 * HwNode::WIRE_LEN];
        let len = encode_snapshot(&mut blob, 1, &nodes);
        assert!(
            snapshot_nodes(&blob[..len - 1]).is_none(),
            "a header promising one more record than the buffer holds"
        );
        // A count whose byte span overflows is refused before any arithmetic
        // on it can wrap.
        let mut overflowing = [0u8; HwTreeHeader::WIRE_LEN];
        overflowing[..HwTreeHeader::WIRE_LEN]
            .copy_from_slice(&HwTreeHeader::new(0, u64::MAX).to_le_bytes());
        assert!(snapshot_nodes(&overflowing).is_none());
        // An empty tree is well-formed and simply yields nothing.
        let header = HwTreeHeader::new(3, 0).to_le_bytes();
        let empty = snapshot_nodes(&header).expect("an empty snapshot is well-formed");
        assert!(empty.is_empty());
        assert_eq!(empty.count(), 0);
    }

    #[test]
    fn an_undecodable_record_is_skipped_never_half_trusted() {
        // A corrupt record can hide a node; it must never invent one, and it
        // must not stop the walk reaching the records behind it.
        let nodes = usb_shaped_tree();
        let mut blob = [0u8; HwTreeHeader::WIRE_LEN + 6 * HwNode::WIRE_LEN];
        let len = encode_snapshot(&mut blob, 1, &nodes);
        // Corrupt the first record's device-class field to a value no class
        // decodes from, leaving every later record intact.
        let class_at = HwTreeHeader::WIRE_LEN + 12;
        blob[class_at] = 11;
        blob[class_at + 1] = 0;
        let read: usize = snapshot_nodes(&blob[..len])
            .expect("the extent is still sound")
            .filter(|node| node.id() == nodes[0].id())
            .count();
        assert_eq!(read, 0, "the corrupt record is not yielded");
        let survivors: usize = snapshot_nodes(&blob[..len])
            .expect("the extent is still sound")
            .count();
        assert_eq!(
            survivors,
            nodes.len() - 1,
            "and every intact record behind it still is"
        );
    }

    #[test]
    fn the_snapshot_ancestor_fold_degrades_safe_on_a_malformed_blob() {
        // A short header, and a header promising more records than the buffer
        // holds, both impose nothing (degrade safe) rather than fabricating a
        // fault from a snapshot that cannot be trusted.
        assert_eq!(
            ancestor_imposed_status_from_snapshot(&[0u8; 4], 3),
            BlkStatus::Ok
        );
        let nodes = usb_shaped_tree();
        let mut blob = [0u8; HwTreeHeader::WIRE_LEN + 6 * HwNode::WIRE_LEN];
        let len = encode_snapshot(&mut blob, 1, &nodes);
        // Truncate the final record: the header still promises 6 nodes.
        assert_eq!(
            ancestor_imposed_status_from_snapshot(&blob[..len - 1], 3),
            BlkStatus::Ok
        );
    }
}
