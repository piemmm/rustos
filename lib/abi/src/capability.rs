//! Capability identifiers as carried across the ABI.
//!
//! A [`CapabilityId`] is the wire representation of a kernel capability. The
//! identifier space is dense and bounded by [`CAPABILITY_ID_MAX`] so that
//! capability sets can be represented as fixed-size bitmaps without an
//! allocator.
//!
//! Values defined here are part of the frozen `abi-v1` contract: existing
//! identifiers may not be re-numbered or removed; new capabilities must take
//! the next free integer and bump [`CAPABILITY_ID_MAX`] if necessary.

use crate::Errno;

/// Inclusive upper bound on capability identifiers in `abi-v1`.
///
/// Sized to leave headroom for the capabilities introduced by later stages
/// without forcing a `CapabilitySet` to grow past a single 64-bit word per
/// 64 entries. Increasing this value is a breaking ABI change.
pub const CAPABILITY_ID_MAX: u16 = 255;

/// Stable identifier for a kernel capability.
///
/// The inner integer is the on-wire representation; the wrapper type prevents
/// accidental confusion with other 16-bit ABI values such as syscall numbers.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct CapabilityId(u16);

impl CapabilityId {
    /// Mount and unmount filesystems.
    pub const FS_MOUNT: Self = Self(1);
    /// Open raw network sockets.
    pub const NET_RAW: Self = Self(2);
    /// Load a driver module in user space.
    pub const DRV_LOAD: Self = Self(3);
    /// Load a driver module in kernel space (additional to `DRV_LOAD`).
    pub const DRV_KERNEL: Self = Self(4);
    /// Create, modify, or delete users.
    pub const USER_ADMIN: Self = Self(5);
    /// Adjust the system wall clock.
    pub const TIME_SET: Self = Self(6);
    /// Bind to privileged IPC endpoints.
    pub const IPC_BIND_PRIVILEGED: Self = Self(7);
    /// Read the security audit log.
    pub const AUDIT_READ: Self = Self(8);
    /// Write entries to the security audit log.
    pub const AUDIT_WRITE: Self = Self(9);
    /// Allocate and free DMA-able memory through the per-process heap.
    ///
    /// Granted to user-space drivers that need to publish buffer
    /// addresses to a bus-master device (virtio-blk, virtio-net,
    /// future `NVMe`). Holders may call the kernel's DMA allocator,
    /// which hands back page-aligned, contiguous-by-physical-address
    /// regions out of the calling process's heap, with guard pages
    /// around the slab and zero-on-free for every byte ever made
    /// device-visible.
    pub const MEM_DMA: Self = Self(10);
    /// Bind to a hardware interrupt line and wait for its wake-ups.
    ///
    /// Granted to user-space drivers whose hardware raises an IRQ the
    /// driver must observe (virtio-blk / virtio-net completion queues,
    /// future NIC / `NVMe` driver interrupts). Holders may call the
    /// `irq_bind` / `irq_wait` syscall pair (`abi-v1` numbers 8 and 9),
    /// which mint an opaque [`crate::IrqHandle`] backed by a per-line
    /// kernel wait queue and block on it with a caller-supplied
    /// timeout. The capability does not grant the ability to *raise*
    /// or *mask* an interrupt line; both remain kernel-only
    /// (capability checks before state touches).
    pub const IRQ_BIND: Self = Self(11);
    /// Map a device's memory-mapped register window into a driver's
    /// address space.
    ///
    /// Granted to user-space bus drivers (`drivers/bus/pcie_brcm`,
    /// `drivers/bus/mmio`) that must read and write a device's
    /// register block (a PCI memory BAR, a virtio-MMIO transport
    /// slot). Holders may call the kernel's MMIO-map facility, which
    /// validates the requested physical region, maps it with caching
    /// disabled (`MapFlags::NO_CACHE`), and hands back a
    /// bounds-checked [`RegisterWindow`](crate::RegisterWindow). The
    /// capability does not let a driver synthesise an arbitrary
    /// pointer: the kernel is the sole minter of a `RegisterWindow`,
    /// so a driver can only reach memory the kernel chose to map for
    /// it (no ambient authority; — capability
    /// checks before state touches).
    pub const MMIO_MAP: Self = Self(12);
    /// Query system information beyond the caller's own principal.
    ///
    /// Required by the System Information API for
    /// queries whose answer spans principals other than the caller —
    /// for example listing every process on the system rather than
    /// only the caller's own. Unprivileged, self-scoped queries ("list
    /// my own processes") require no capability; this one gates the
    /// global view (capability checks before state
    /// touches).
    pub const SYSINFO_GLOBAL: Self = Self(13);
    /// Query kernel-internal system information.
    ///
    /// Required by the System Information API for
    /// queries that expose kernel-internal state — for example kernel
    /// memory statistics — which a global-but-unprivileged observer
    /// must not see.
    pub const SYSINFO_KERNEL: Self = Self(14);
    /// Read the detected hardware tree through the System Information
    /// API.
    ///
    /// Required by the privileged hardware-tree query: the tree is exposed read-only to tools through the
    /// System Information API, and there is no path that bypasses this
    /// capability check.
    pub const SYSINFO_HW: Self = Self(15);
    /// Read the monotonic clock at full nanosecond resolution.
    ///
    /// `clock_get` (`abi-v1` syscall 7) is callable by every task, but
    /// a high-resolution timer is a building block for cache- and
    /// execution-timing side-channel attacks.
    /// Callers that do not hold this capability — in particular the
    /// parser sandboxes and untrusted `userland/apps` — receive
    /// a value coarsened to
    /// [`COARSE_CLOCK_GRANULARITY_NS`](crate::COARSE_CLOCK_GRANULARITY_NS),
    /// so the precise timer is available only to principals explicitly
    /// trusted with it (security by default).
    pub const TIME_HIRES: Self = Self(16);
    /// Spawn a new process: build a fresh user address space from a
    /// validated `rxe` image and drop into it in user mode.
    ///
    /// Spawning a program is a privileged operation — it materialises a
    /// new principal's address space and hands it the CPU — so it is
    /// gated rather than ambient (no ambient authority;
    /// — capability checks before state touches). The kernel-side
    /// spawn caller (`kernel/core`) verifies this capability and audits
    /// the decision before building the image; the memory mechanism in
    /// `kernel/mem` stays capability-agnostic. The hosted
    /// program still receives only the intersection of its own signed
    /// manifest request and its user's grants.
    pub const PROC_SPAWN: Self = Self(17);
    /// Use a console-backed standard *output* stream.
    ///
    /// The coarse gate on the `stream_write` syscall (`abi-v1` number 11)
    /// when the addressed descriptor's backing is the privileged
    /// *hardware* console — the detected framebuffer when present, else
    /// the first discovered UART (`plans/PI.md` P6). The fine, per-fd
    /// authority is the inherited descriptor table the spawner
    /// established ([`crate::DescriptorTable`]); this capability says
    /// the principal may use a *console-backed* output stream at all.
    /// Only the early bring-up principals (PID 1 `init`, login, getty,
    /// the shell) are granted it, so an ordinary app cannot scribble on
    /// the system console (no ambient authority; —
    /// capability checks before state touches).
    pub const CONSOLE_WRITE: Self = Self(18);
    /// Use a console-backed standard *input* stream.
    ///
    /// The coarse gate on the `stream_read` syscall (`abi-v1` number 13)
    /// when the addressed descriptor's backing is the privileged
    /// *hardware* console input — the first discovered keyboard/UART input
    /// source (`plans/PI.md` P6). The input counterpart of
    /// [`CONSOLE_WRITE`](Self::CONSOLE_WRITE); the fine, per-fd authority
    /// is the inherited descriptor table ([`crate::DescriptorTable`]). Only the early bring-up principals (PID 1 `init`, login,
    /// getty, the shell) are granted it, so an ordinary app cannot read
    /// the system console (no ambient authority; —
    /// capability checks before state touches).
    pub const CONSOLE_READ: Self = Self(19);
    /// Raise a hard resource limit above its inherited ceiling.
    ///
    /// A process may always *lower* its own soft or hard resource bounds
    /// ([`crate::ResourceLimit`]) without any capability, but *raising* a
    /// hard bound — or setting any bound above the ceiling it inherited —
    /// is the privileged operation this capability gates (
    /// the resource-limit analogue of the "never widen on delegation"
    /// rule). The `rlimit_set` syscall (`abi-v1` number 18) refuses such a
    /// request with [`Errno::PermissionDenied`] unless the caller holds this
    /// capability (capability checks before state
    /// touches; — no ambient authority).
    pub const RLIMIT_RAISE: Self = Self(20);
    /// Read the system user database (`/System/Security/Users`) through the `users_db_read` syscall
    /// (`abi-v1` number 19).
    ///
    /// The database carries every account's identity and salted password
    /// record, so reading it is privileged rather than ambient
    /// (no ambient authority; — the on-disk record
    /// is itself permission-checked). Only the authentication principal
    /// (login) is granted it: login verifies offered credentials against
    /// the delivered records and drops them immediately (secret hygiene). An ordinary app can neither enumerate accounts
    /// nor see a password record (capability checks
    /// before state touches).
    pub const USERS_READ: Self = Self(21);
    /// Inject decoded keystroke input into a system text console
    /// (`plans/PI.md` P11 — keyboard input for the video
    /// console).
    ///
    /// The gate on the `console_input` syscall (`abi-v1` number 22): an
    /// input driver that has decoded a directly attached keyboard
    /// (USB-HID / PS-2) into a stream of console bytes pushes them into a
    /// target console's kernel-side input queue, which a
    /// [`SyscallNumber::STREAM_READ`](crate::SyscallNumber::STREAM_READ)
    /// of that console then drains. Feeding the system console's input is
    /// privileged rather than ambient (no ambient
    /// authority): only the keyboard-input driver the device manager
    /// loaded for the discovered keyboard node is granted it, so an
    /// ordinary task cannot forge keystrokes into another session's login
    /// (capability checks before state touches). It is
    /// the producer counterpart of [`CONSOLE_READ`](Self::CONSOLE_READ),
    /// which gates the *consumer* (login) of the same console.
    pub const INPUT_INJECT: Self = Self(22);
    /// Acquire ownership of the display (the framebuffer seat) and, with
    /// it, the keyboard input focus (`plans/PI.md`
    /// P11 — input follows the surface owner).
    ///
    /// The gate on the `display_acquire` / `display_release` syscalls
    /// (`abi-v1` numbers 23 / 24): the compositing window manager holds
    /// this capability and acquires the display when it takes over the
    /// screen. Acquiring claims the kernel input-focus arbiter's
    /// foreground (decoded key events route to the desktop keyboard
    /// channel instead of the text console); releasing returns focus to
    /// the text console, so input follows the surface owner automatically
    /// — the desktop analogue of "input follows the foreground tty". Owning the display is privileged rather than
    /// ambient (no ambient authority; — capability
    /// checks before state touches): only a session's window manager is
    /// granted it, so an ordinary task cannot seize the screen or steal
    /// keyboard focus from the active session.
    pub const DISPLAY: Self = Self(23);
    /// Read decoded keyboard events from the kernel keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// The gate on the `keyboard_read` syscall (`abi-v1` number 25): the
    /// principal that owns the display (the window manager / desktop
    /// session) drains framed [`crate::input::KeyInput`] records the
    /// kernel input-focus arbiter routed to it while it held focus. It is
    /// the desktop counterpart of [`CONSOLE_READ`](Self::CONSOLE_READ),
    /// which gates the *text* console's consumer (login): a keyboard
    /// stream is delivered only to whoever currently owns the surface, and
    /// reading it is privileged rather than ambient (
    /// — capability checks before state touches; — bind to streams,
    /// never to a device). An unattached channel denies rather than
    /// leaking, so a task without the capability — or one reading when the
    /// arbiter holds no desktop focus — sees nothing.
    pub const INPUT_READ: Self = Self(24);
    /// Call the user-space firmware property-mailbox service
    /// (`plans/PI.md` P10 D3).
    ///
    /// The send-side gate on the `VideoCore` mailbox call endpoint
    /// ([`crate::mailbox_ipc::MAILBOX_ENDPOINT`]): a driver that needs a
    /// firmware property exchange — e.g. the VL805 USB firmware reload
    /// (`drivers/bus/usb/vl805`) — holds this capability, and the
    /// `vcmailbox` service creates the endpoint with it as the required
    /// sender capability. The mailbox reconfigures hardware (framebuffer,
    /// clocks, PCIe firmware), so reaching it is privileged rather than
    /// ambient (no ambient authority; — capability
    /// checks before state touches): an ordinary task cannot drive the
    /// firmware mailbox.
    pub const MAILBOX: Self = Self(25);
    /// Emit a structured diagnostic record to the system log through the
    /// `log_emit` syscall (`abi-v1` number 36).
    ///
    /// The gate on the user-space logging path: a holder hands the kernel a
    /// bounded, validated [`crate::LogRecordRef`] which the kernel attributes
    /// to the calling task and emits through its **diagnostic** log sink (the
    /// serial UART on a debug build, the video console on release). This is
    /// **not** the hash-chained security audit log — that channel
    /// ([`AUDIT_WRITE`](Self::AUDIT_WRITE)) stays kernel-only, so a holder of
    /// this capability can never write, forge, or truncate an audit entry. Emitting to the system console is privileged
    /// rather than ambient (no ambient authority; —
    /// capability checks before state touches): only trusted system services
    /// (the device manager, login) are granted it, so an ordinary app cannot
    /// scribble diagnostics onto the captured serial line.
    pub const LOG_EMIT: Self = Self(26);
    /// Publish a discovered child device node into the live hardware tree
    /// through the `hw_emit_node` syscall (`abi-v1` number 37).
    ///
    /// The gate on recursive, user-space hardware discovery: a user-space
    /// **bus** driver (a PCIe root complex, a USB host) enumerates the
    /// devices behind it and emits each as a child [`crate::HwNode`] so the
    /// device manager autoloads the matching driver in turn (discovery is data-driven, never a compiled-in list). It confers
    /// **no** authority by itself: the kernel admits an emitted node only
    /// when every [`crate::hwtree::HwResource`] it requests is wholly
    /// contained within a device-resource grant the emitting driver already
    /// holds, so a bus driver can never mint a child more authority than it
    /// was granted (no ambient authority; — capability
    /// and bound checks before state touches; — a driver receives only
    /// its matched node's resources). Publishing into the global hardware
    /// inventory is privileged rather than ambient: only an autoloaded bus
    /// driver is granted it, never an ordinary task.
    pub const HW_EMIT: Self = Self(27);

    /// Every capability assigned a canonical name in `abi-v1`, paired with
    /// that name.
    ///
    /// This table is the **single source of truth** for both
    /// [`name`](Self::name) and [`from_name`](Self::from_name), so the two
    /// can never disagree on the name↔id mapping. The
    /// `CAP_*` names are the ones the charter uses throughout and are part of the frozen `abi-v1` contract: an existing
    /// name may not be re-spelled or re-pointed, and a newly assigned
    /// identifier takes a new row.
    const NAMED: &'static [(Self, &'static str)] = &[
        (Self::FS_MOUNT, "CAP_FS_MOUNT"),
        (Self::NET_RAW, "CAP_NET_RAW"),
        (Self::DRV_LOAD, "CAP_DRV_LOAD"),
        (Self::DRV_KERNEL, "CAP_DRV_KERNEL"),
        (Self::USER_ADMIN, "CAP_USER_ADMIN"),
        (Self::TIME_SET, "CAP_TIME_SET"),
        (Self::IPC_BIND_PRIVILEGED, "CAP_IPC_BIND_PRIVILEGED"),
        (Self::AUDIT_READ, "CAP_AUDIT_READ"),
        (Self::AUDIT_WRITE, "CAP_AUDIT_WRITE"),
        (Self::MEM_DMA, "CAP_MEM_DMA"),
        (Self::IRQ_BIND, "CAP_IRQ_BIND"),
        (Self::MMIO_MAP, "CAP_MMIO_MAP"),
        (Self::SYSINFO_GLOBAL, "CAP_SYSINFO_GLOBAL"),
        (Self::SYSINFO_KERNEL, "CAP_SYSINFO_KERNEL"),
        (Self::SYSINFO_HW, "CAP_SYSINFO_HW"),
        (Self::TIME_HIRES, "CAP_TIME_HIRES"),
        (Self::PROC_SPAWN, "CAP_PROC_SPAWN"),
        (Self::CONSOLE_WRITE, "CAP_CONSOLE_WRITE"),
        (Self::CONSOLE_READ, "CAP_CONSOLE_READ"),
        (Self::RLIMIT_RAISE, "CAP_RLIMIT_RAISE"),
        (Self::USERS_READ, "CAP_USERS_READ"),
        (Self::INPUT_INJECT, "CAP_INPUT_INJECT"),
        (Self::DISPLAY, "CAP_DISPLAY"),
        (Self::INPUT_READ, "CAP_INPUT_READ"),
        (Self::MAILBOX, "CAP_MAILBOX"),
        (Self::LOG_EMIT, "CAP_LOG_EMIT"),
        (Self::HW_EMIT, "CAP_HW_EMIT"),
    ];

    /// The canonical `CAP_*` name of this capability, or [`None`] for an
    /// in-range identifier that `abi-v1` has not yet assigned a name.
    ///
    /// The returned string is the exact spelling [`from_name`](Self::from_name)
    /// accepts, so a name round-trips back to the same identifier.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(cap, _)| *cap == self)
            .map(|(_, name)| *name)
    }

    /// The capability with canonical `CAP_*` name `name`, or [`None`] if no
    /// `abi-v1` capability bears that name.
    ///
    /// The match is exact and case-sensitive; there is no abbreviation or
    /// alias, so a name either denotes exactly one frozen capability or
    /// nothing at all (fail closed).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::NAMED
            .iter()
            .find(|(_, candidate)| *candidate == name)
            .map(|(cap, _)| *cap)
    }

    /// Construct a [`CapabilityId`] from its raw value, validating the range.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`CAPABILITY_ID_MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > CAPABILITY_ID_MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Position of this capability inside a 256-bit capability set.
    ///
    /// Always less than 256 by construction.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Read-only membership test over a principal's granted capabilities.
///
/// The set's concrete representation (`CapabilitySet` and its 256-bit
/// bitmap) lives in `lib/caps`, which depends on this crate. ABI-level
/// host seams — for example `VirtioHostFactory` in `lib/virtio` — must
/// gate on a granted capability without naming `lib/caps`, because the reverse
/// edge `lib/abi -> lib/caps` would invert the `lib/*` layering. They therefore accept `&dyn CapabilityQuery`;
/// `lib/caps` implements this for its `CapabilitySet`.
///
/// The trait is object-safe so a seam can hold a `&dyn CapabilityQuery`
/// without monomorphising over the caller's set type.
pub trait CapabilityQuery {
    /// `true` if the queried principal has been granted `cap`.
    fn holds(&self, cap: CapabilityId) -> bool;
}

#[cfg(test)]
mod tests {
    use super::{CapabilityId, CapabilityQuery, CAPABILITY_ID_MAX};
    use crate::Errno;

    /// Minimal `CapabilityQuery` that grants exactly one capability,
    /// proving the trait is object-safe and usable behind `&dyn`.
    struct OneCap(CapabilityId);
    impl CapabilityQuery for OneCap {
        fn holds(&self, cap: CapabilityId) -> bool {
            cap == self.0
        }
    }

    #[test]
    fn capability_query_is_object_safe_and_answers() {
        let query: &dyn CapabilityQuery = &OneCap(CapabilityId::MEM_DMA);
        assert!(query.holds(CapabilityId::MEM_DMA));
        assert!(!query.holds(CapabilityId::NET_RAW));
    }

    #[test]
    fn well_known_ids_are_frozen() {
        // The numeric values are part of abi-v1; do not renumber.
        assert_eq!(CapabilityId::FS_MOUNT.as_u16(), 1);
        assert_eq!(CapabilityId::NET_RAW.as_u16(), 2);
        assert_eq!(CapabilityId::DRV_LOAD.as_u16(), 3);
        assert_eq!(CapabilityId::DRV_KERNEL.as_u16(), 4);
        assert_eq!(CapabilityId::USER_ADMIN.as_u16(), 5);
        assert_eq!(CapabilityId::TIME_SET.as_u16(), 6);
        assert_eq!(CapabilityId::IPC_BIND_PRIVILEGED.as_u16(), 7);
        assert_eq!(CapabilityId::AUDIT_READ.as_u16(), 8);
        assert_eq!(CapabilityId::AUDIT_WRITE.as_u16(), 9);
        assert_eq!(CapabilityId::MEM_DMA.as_u16(), 10);
        assert_eq!(CapabilityId::IRQ_BIND.as_u16(), 11);
        assert_eq!(CapabilityId::MMIO_MAP.as_u16(), 12);
        assert_eq!(CapabilityId::SYSINFO_GLOBAL.as_u16(), 13);
        assert_eq!(CapabilityId::SYSINFO_KERNEL.as_u16(), 14);
        assert_eq!(CapabilityId::SYSINFO_HW.as_u16(), 15);
        assert_eq!(CapabilityId::TIME_HIRES.as_u16(), 16);
        assert_eq!(CapabilityId::PROC_SPAWN.as_u16(), 17);
        assert_eq!(CapabilityId::CONSOLE_WRITE.as_u16(), 18);
        assert_eq!(CapabilityId::CONSOLE_READ.as_u16(), 19);
        assert_eq!(CapabilityId::RLIMIT_RAISE.as_u16(), 20);
        assert_eq!(CapabilityId::USERS_READ.as_u16(), 21);
        assert_eq!(CapabilityId::INPUT_INJECT.as_u16(), 22);
        assert_eq!(CapabilityId::DISPLAY.as_u16(), 23);
        assert_eq!(CapabilityId::INPUT_READ.as_u16(), 24);
        assert_eq!(CapabilityId::MAILBOX.as_u16(), 25);
        assert_eq!(CapabilityId::LOG_EMIT.as_u16(), 26);
        assert_eq!(CapabilityId::HW_EMIT.as_u16(), 27);
    }

    #[test]
    fn names_are_frozen_and_round_trip() {
        // The canonical `CAP_*` spellings are part of abi-v1; do not
        // re-spell or re-point them.
        assert_eq!(CapabilityId::FS_MOUNT.name(), Some("CAP_FS_MOUNT"));
        assert_eq!(CapabilityId::AUDIT_READ.name(), Some("CAP_AUDIT_READ"));
        assert_eq!(CapabilityId::SYSINFO_HW.name(), Some("CAP_SYSINFO_HW"));
        assert_eq!(CapabilityId::TIME_HIRES.name(), Some("CAP_TIME_HIRES"));
        assert_eq!(CapabilityId::PROC_SPAWN.name(), Some("CAP_PROC_SPAWN"));
        assert_eq!(
            CapabilityId::CONSOLE_WRITE.name(),
            Some("CAP_CONSOLE_WRITE")
        );
        assert_eq!(CapabilityId::CONSOLE_READ.name(), Some("CAP_CONSOLE_READ"));
        assert_eq!(CapabilityId::RLIMIT_RAISE.name(), Some("CAP_RLIMIT_RAISE"));
        assert_eq!(CapabilityId::USERS_READ.name(), Some("CAP_USERS_READ"));
        assert_eq!(CapabilityId::INPUT_INJECT.name(), Some("CAP_INPUT_INJECT"));
        assert_eq!(CapabilityId::DISPLAY.name(), Some("CAP_DISPLAY"));
        assert_eq!(CapabilityId::INPUT_READ.name(), Some("CAP_INPUT_READ"));

        // Every named id round-trips name -> id -> name.
        for &(cap, name) in CapabilityId::NAMED {
            assert_eq!(cap.name(), Some(name));
            assert_eq!(CapabilityId::from_name(name), Some(cap));
        }
    }

    #[test]
    fn every_assigned_id_has_a_name() {
        // Capabilities 1..=27 are assigned in abi-v1; each must carry a
        // canonical name so `getcap`/`setcap` can render and accept it.
        for raw in 1..=27 {
            let cap = CapabilityId::from_raw(raw).expect("in range");
            assert!(cap.name().is_some(), "capability {raw} has no name");
        }
    }

    #[test]
    fn from_name_is_exact_and_fails_closed() {
        // Unknown, mis-cased, or differently-spelled names denote nothing.
        assert_eq!(CapabilityId::from_name(""), None);
        assert_eq!(CapabilityId::from_name("FS_MOUNT"), None);
        assert_eq!(CapabilityId::from_name("cap_fs_mount"), None);
        assert_eq!(CapabilityId::from_name("CAP_FS_MOUNT "), None);
        assert_eq!(CapabilityId::from_name("CAP_NOPE"), None);
    }

    #[test]
    fn an_unassigned_in_range_id_has_no_name() {
        let unassigned = CapabilityId::from_raw(200).expect("in range");
        assert_eq!(unassigned.name(), None);
    }

    #[test]
    fn from_raw_rejects_out_of_range() {
        assert_eq!(CapabilityId::from_raw(0).map(CapabilityId::as_u16), Ok(0));
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX).map(CapabilityId::as_u16),
            Ok(CAPABILITY_ID_MAX),
        );
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX + 1),
            Err(Errno::OutOfRange),
        );
    }

    #[test]
    fn index_is_within_bitset_bounds() {
        assert!(CapabilityId::AUDIT_WRITE.index() < 256);
        assert!(CapabilityId::MEM_DMA.index() < 256);
        assert!(CapabilityId::IRQ_BIND.index() < 256);
        assert!(CapabilityId::MMIO_MAP.index() < 256);
        assert!(CapabilityId::SYSINFO_GLOBAL.index() < 256);
        assert!(CapabilityId::SYSINFO_KERNEL.index() < 256);
        assert!(CapabilityId::SYSINFO_HW.index() < 256);
        assert!(CapabilityId::TIME_HIRES.index() < 256);
    }
}
