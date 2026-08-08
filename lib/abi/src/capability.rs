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
    /// Acquire ownership of the seat — the display with its keyboard —
    /// as an exclusive, owner-tracked lease (`plans/DISPLAY.md`;
    /// `plans/PI.md` P11 — input follows the surface owner).
    ///
    /// The gate on the `display_acquire` / `display_release` syscalls
    /// (`abi-v1` numbers 23 / 24): the compositing window manager holds
    /// this capability and acquires the seat when it takes over the
    /// screen. The kernel records the **kernel-attested caller** as the
    /// seat owner and checks that owner on every ownership-changing call:
    /// a `display_acquire` while another task holds the seat is refused
    /// (`SeatBusy`) rather than displacing the holder — even when both
    /// principals legitimately hold this capability — and a
    /// `display_release` by anyone but the owner is refused
    /// (`SeatNotOwner`), so "cannot steal focus from the active session"
    /// is an enforced kernel invariant, not a grant-policy side effect.
    /// While the seat is held, decoded key events route to the owner's
    /// desktop keyboard channel; the owner's release returns them to the
    /// text console — the desktop analogue of "input follows the
    /// foreground tty". The lease is revocable: a seat administrator
    /// (`plans/DISPLAY.md` D3) can evict a wedged owner, whose next
    /// owner-gated call then sees the distinct `SeatRevoked` refusal.
    pub const DISPLAY: Self = Self(23);
    /// Read decoded keyboard events from the kernel keyboard channel
    /// (`plans/PI.md` P11 — keyboard input for the
    /// desktop).
    ///
    /// The gate on the `keyboard_read` syscall (`abi-v1` number 25): the
    /// task that owns the seat (the window manager / desktop session)
    /// drains framed [`crate::input::KeyInput`] records the kernel seat
    /// registry routed to it while it held the seat. It is
    /// the desktop counterpart of [`CONSOLE_READ`](Self::CONSOLE_READ),
    /// which gates the *text* console's consumer (login). The capability
    /// alone is not enough: the drain is additionally **owner-gated**
    /// against the seat's live lease, so a second `CAP_INPUT_READ` holder
    /// that does not own the seat is refused (`SeatNotOwner`, or
    /// `SeatRevoked` after an administrative eviction) and can never
    /// siphon another session's keystrokes (capability and owner checks
    /// before state touches; bind to streams, never to a device). An
    /// unattached channel denies rather than leaking.
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
    /// this capability can never write, forge, or truncate an audit entry, and
    /// the kernel attributes every record to the calling task, so one cannot
    /// be mis-attributed. Emitting is capability-gated rather than ambient (no
    /// ambient authority; capability checks before state touches), but it is
    /// part of the interactive account baseline
    /// (`tairix_users::SESSION_BASELINE`), not a service-only grant: a session
    /// legitimately reports its own operational state. The accepted cost is
    /// that any program a logged-in user runs may write to the machine-wide
    /// diagnostic log — noise and provenance confusion are possible, and a
    /// debug build's captured serial line is user-writable. A program still
    /// receives it only if its own manifest requests it, intersected with the
    /// account's ceiling.
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
    /// Participate in per-endpoint-granted synchronous call IPC: create a
    /// grant-restricted call endpoint, and submit calls to one.
    ///
    /// A *grant-restricted* call endpoint is one whose authority is not a
    /// process-wide capability *class* but a per-endpoint, region-scoped
    /// **grant** keyed to the endpoint's id — the same device-resource grant
    /// machinery that scopes an MMIO window or an IRQ line to one driver
    /// (no ambient authority). It is the primitive a host-controller driver
    /// uses to serve one private call endpoint per device it enumerates (the
    /// USB request-block transport, `plans/USB.md`): the server creates the
    /// endpoint (and the kernel mints it the matching
    /// [`HwResource::endpoint`](crate::hwtree::HwResource::endpoint) grant, so
    /// it may forward the endpoint onto a child node it publishes), and only a
    /// task the kernel later grants that exact endpoint — the autoloaded class
    /// driver bound to the emitted device node — may call it.
    ///
    /// Holding this capability alone confers no reach: a caller may submit to
    /// a grant-restricted endpoint only if it *also* holds the per-endpoint
    /// grant, so two class drivers behind one controller cannot reach each
    /// other's endpoint even though both hold the class capability (capability
    /// and per-endpoint grant checks before state touches; — a driver
    /// receives only its matched node's resources).
    pub const IPC_ENDPOINT: Self = Self(28);
    /// Participate in cross-process shared memory: create a shared-memory
    /// region, and map a region the kernel has granted you.
    ///
    /// A shared-memory region is a block of kernel-owned RAM two cooperating
    /// processes map into their own address spaces to exchange bulk data
    /// without a kernel copy. Reach is **not** a process-wide capability
    /// *class* but a per-region, id-scoped **grant** — the same
    /// device-resource grant machinery that scopes an MMIO window, an IRQ
    /// line, or a call endpoint to one driver (no ambient authority). It is
    /// the data-buffer half of the USB request-block transport
    /// (`plans/USB.md`): a host-controller driver creates one region per
    /// device it serves (the kernel mints it the matching
    /// [`HwResource::shared`](crate::hwtree::HwResource::shared) grant, so it
    /// may forward the region onto a child node it publishes), and only a
    /// task the kernel later grants that exact region — the autoloaded class
    /// driver bound to the emitted device node — may map it.
    ///
    /// Holding this capability alone confers no reach: mapping a region
    /// requires *also* holding the per-region grant the kernel resolves
    /// against the calling task, so two class drivers behind one controller
    /// cannot reach each other's buffer even though both hold the class
    /// capability (capability and per-region grant checks before state
    /// touches; — a driver receives only its matched node's resources).
    /// Creating a region is gated by this capability so a region is never
    /// minted ambiently.
    pub const SHM: Self = Self(29);
    /// Open, read, write, create, and remove files and directories by path
    /// through the filesystem syscalls (`fs_open` and friends, `abi-v1`
    /// numbers 46..=56).
    ///
    /// The coarse entry gate on the userland filesystem surface: a holder
    /// may *attempt* a path operation, but the authority over any one path
    /// remains the per-inode owner/mode/ACL/`required_cap` check the VFS
    /// applies under the caller's real `Credentials` (the kernel supplies
    /// identity, never the caller). Holding it confers no blanket reach —
    /// a file the inode model denies is still refused — so it is the
    /// "may use the filesystem at all" gate, not ambient authority
    /// (no ambient authority; capability checks before state touches).
    /// Mount flags (`ro`/`nosuid`/`nodev`/`noexec`) are honoured
    /// independently. The early bring-up and ordinary file-using
    /// principals (login, the shell, services, apps) hold it; a sandboxed
    /// parser that should reach no filesystem does not.
    pub const FS_ACCESS: Self = Self(30);
    /// Spawn a new process **as a different user** — drop the child into a
    /// target `(uid, gid, supplementary groups)` credential resolved by the
    /// kernel from the authoritative identity table, rather than inheriting
    /// the caller's own credential.
    ///
    /// This gates the one privileged transition in the spawn-as-user model:
    /// a running process can never mutate its own identity (there is no
    /// setuid-self), so the *only* way a task's credential changes is at
    /// process creation, by a holder of this capability asking the kernel to
    /// start a child under a defined user. Absent it, `spawn` can only ever
    /// hand the child the caller's own inherited credential — never elevate
    /// or switch user (no ambient authority; fail closed). Its sole intended
    /// holder is the privileged session manager (`login`), which authenticates
    /// a user and then starts their shell under the authenticated identity.
    /// The kernel resolves the full credential from the identity table it
    /// vouches for, so the caller chooses *which* user but never fabricates
    /// the groups or the identity itself.
    pub const SPAWN_AS_USER: Self = Self(31);
    /// Read the **unfiltered, global** kernel introspection view — the live
    /// process table, kernel memory accounting, the mount table, machine
    /// identity, uptime, and any task's resource limits — through the single
    /// privileged `sysinfo_introspect` syscall.
    ///
    /// This gates the one kernel primitive that answers with the *whole*
    /// system's state, never narrowed by principal. Its sole intended holder
    /// is the user-space System Information service (`sysinfod`), which is the
    /// trusted scoping broker: it re-derives every per-client scope (self vs
    /// global, the `CAP_SYSINFO_GLOBAL`/`CAP_SYSINFO_KERNEL` client gates)
    /// against each requester's kernel-attested `Origin` before returning any
    /// subset of the global view. Keeping the kernel primitive minimal and
    /// global — and the policy in the audited userland broker — holds the
    /// ring-0 attack surface down while the kernel stays the identity
    /// authority (no ambient authority; fail closed).
    pub const SYSINFO_INTROSPECT: Self = Self(32);
    /// Administer **all seats**: switch which session is foreground across
    /// every seat and forcibly revoke another principal's seat lease
    /// (`seat_switch` / `seat_revoke`, `plans/DISPLAY.md` D3).
    ///
    /// This is the seat-multiplexing authority — the `chvt`/`logind`
    /// equivalent — and it guards a *group* of resources (every seat, every
    /// session's focus), not one surface: `CAP_DISPLAY` owns a single seat's
    /// lease, the `CAP_INPUT_*` pair route input, and none of them can evict
    /// another principal's lease or retarget a seat's foreground console.
    /// Its sole intended holder is the seat-manager service (`seatmgr`),
    /// introduced in the same change as the two syscalls that enforce this
    /// capability. Every switch and revoke is a security-relevant ownership
    /// change and is audit-logged with a stable event id; the evicted
    /// owner's next owner-gated call fails closed with the distinct
    /// `SeatRevoked` refusal, so the loss is observable, never silent.
    pub const SEAT_ADMIN: Self = Self(33);
    /// Exempt the calling process's anonymous memory from the swap tiers
    /// (`plans/STRESSTEST.md` ST2).
    ///
    /// Gates the `mem_pin` syscall, which marks the caller's entire
    /// anonymous memory — current and future — ineligible for the
    /// compressed `ramzip` tier and any future lower swap tier. Exempting
    /// memory from pressure management is a system-wide denial-of-service
    /// lever (pinned bytes can never be reclaimed by compression), so it
    /// is a guarded class of authority no existing capability expresses:
    /// the `CAP_SYSINFO_*` family only observes, and the rlimit facility
    /// only bounds. The pin is additionally bounded by the holder's
    /// effective [`crate::LimitKind::PinnedMemoryBytes`] limit and every
    /// pin/unpin edge is audited. Intended holders are the monitoring and
    /// load-generation tools (`sysmon`, `stress`) whose controlling state
    /// must never stall on its own fault-in under the very pressure they
    /// exist to provoke and observe.
    pub const MEM_PIN: Self = Self(34);

    /// Administer the network stack (`plans/NETWORK.md` §3).
    ///
    /// Gates the `netstack` service's admin surface: interface,
    /// address, and route mutation, plus the per-interface counter
    /// reads that surface alongside them. It guards the whole class of
    /// network-configuration authority — every managed interface, not
    /// one device — and no existing capability expresses it:
    /// `CAP_NET_RAW` moves raw frames, and the `CAP_SYSINFO_*` family
    /// only observes. Enforced by the `netstack` dispatcher against
    /// the caller's kernel-attested origin before any state is
    /// touched; every refusal is audited. Intended holders are the
    /// administrative account ceiling and the network-configuration
    /// tooling.
    pub const NET_ADMIN: Self = Self(35);

    /// Originate transport-layer network traffic (`plans/NETWORK.md` §0).
    ///
    /// Gates the `netstack` socket surface: opening a datagram socket and
    /// the send/bind/connect/join operations that follow from it — the
    /// whole class of *ordinary* network use, "may this principal put
    /// transport traffic on the wire and receive it". It guards a class
    /// of authority, not one endpoint or port, and no existing capability
    /// expresses it: `CAP_NET_RAW` grants unmediated raw-frame access (a
    /// strictly higher authority that bypasses the stack's state
    /// machines), `CAP_NET_ADMIN` reconfigures interfaces, and the
    /// `CAP_SYSINFO_*` family only observes. Binding a listening port
    /// below the privileged-port boundary is a *further* gate
    /// (`CAP_NET_BIND_PRIVILEGED`, a later increment), never folded into
    /// this one. Enforced by the `netstack` socket dispatcher against the
    /// caller's kernel-attested origin before any socket state is touched;
    /// every refusal is a typed error and an audited event. Its intended
    /// holders are ordinary network-using applications, granted through
    /// their signed manifests.
    pub const NET: Self = Self(36);

    /// Enter the strict-priority **real-time** scheduling class
    /// (`SchedClass::Realtime`), so the calling task is dispatched ahead of
    /// every time-shared task on its CPU and is never preempted by one
    /// (`plans/USB.md`; the `sched_set_realtime` syscall).
    ///
    /// Gates the one operation that lets a task escape fair scheduling: an
    /// interrupt-serving user-space driver (the USB host controller, and any
    /// driver whose device raises an IRQ it must service before its hardware
    /// ring drains) elevates itself so a CPU-bound workload can never delay
    /// its wake — the microkernel analogue of a threaded-IRQ / `SCHED_FIFO`
    /// grant. It guards a whole class of authority (the ability to preempt
    /// every ordinary task, system-wide) that no existing capability
    /// expresses: the `CAP_DRV_*`, `CAP_IRQ_BIND`, `CAP_MMIO_MAP`, and
    /// `CAP_MEM_DMA` grants let a driver *reach* its hardware but say nothing
    /// about its scheduling priority, and the `CAP_RLIMIT_RAISE` /
    /// `CAP_MEM_PIN` levers bound or exempt resources without conferring
    /// strict priority. A real-time task that never blocks can monopolise
    /// its CPU against time-shared work, so the class is a guarded privilege
    /// granted only to trusted, IRQ-driven drivers through their signed
    /// manifests, never ambient (fail closed; capability checks before state
    /// touches). Enforced kernel-side at the `sched_set_realtime` syscall
    /// boundary and audited per call.
    pub const SCHED_REALTIME: Self = Self(37);

    /// Bind a *listening* socket to a privileged (well-known) port
    /// (`plans/NETWORK.md` §0, N6b-2).
    ///
    /// Gates the one operation the ordinary [`NET`](Self::NET) grant does
    /// not confer: binding a socket to a local port at or below
    /// [`SOCKET_PRIVILEGED_PORT_MAX`](crate::net::SOCKET_PRIVILEGED_PORT_MAX)
    /// so it may passively accept connections there. The low ports name
    /// well-known services, so squatting one lets a process impersonate a
    /// system service; this capability guards that whole class of
    /// authority (every privileged port, not one), and no existing
    /// capability expresses it — `CAP_NET` grants ordinary transport use
    /// (outbound flows and ephemeral binds) but is deliberately *not*
    /// sufficient to claim a well-known port, exactly as the `CAP_NET`
    /// docs state. Enforced by the `netstack` socket dispatcher against
    /// the caller's kernel-attested origin before any bind state is
    /// touched; every refusal is a typed error and an audited event. Its
    /// intended holders are the system network services, granted through
    /// their signed manifests.
    pub const NET_BIND_PRIVILEGED: Self = Self(38);

    /// Reassign the owning **user** of a filesystem node, and set its
    /// owning **group** beyond what an ordinary owner may (the `chown(2)`
    /// privilege; the Unix `CAP_CHOWN` analogue).
    ///
    /// Changing a file's owner is a security-sensitive act — it can hand a
    /// file to another principal or seize one — so it is a guarded class of
    /// authority, not ambient. This capability guards the whole class of
    /// ownership reassignment (every node, not one path), and no existing
    /// capability expresses it at that granularity: `CAP_FS_ACCESS` is the
    /// coarse "may use the filesystem at all" gate every session holds, and
    /// `CAP_USER_ADMIN` administers the *account database*, not the owner
    /// field of arbitrary inodes. The `fs_set_owner` syscall enforces it
    /// kernel-side against the caller's kernel-attested credential before any
    /// state is touched, and audits the decision.
    ///
    /// The capability is required only to change the **uid**, or to set a
    /// **gid** the caller does not otherwise qualify for. Without it, an
    /// ordinary owner may still set a file's group to one of *their own*
    /// groups — the standard unprivileged `chown :group` — because that
    /// grants no authority the caller does not already have. Any successful
    /// ownership change clears the setuid and setgid bits, so a reassigned
    /// file can never become a new setuid-to-someone-else escalation
    /// (fail closed; capability checks before state touches).
    pub const FS_CHOWN: Self = Self(39);

    /// Signal a process belonging to **another** user principal — the
    /// cross-principal path of the `signal` syscall (`abi-v1` number 64).
    ///
    /// `signal` already lets a process control its own live children, and a
    /// process may always signal another process **it itself owns** (the
    /// same kernel-attested uid) without any capability at all — neither
    /// case needs this grant. This capability gates only what remains:
    /// delivering a control signal to a task owned by a different
    /// principal, which is otherwise refused (fail closed). It guards a
    /// whole class of authority — every other principal's processes, not
    /// one target — and no existing capability expresses it at that
    /// granularity: `CAP_PROC_SPAWN` only creates a process, and
    /// `CAP_USER_ADMIN` administers the account database, never a running
    /// task. The `signal` handler checks it only after the caller's own
    /// children and the caller's own uid have both been ruled out
    /// (capability checks before state touches), and audits the decision.
    pub const PROC_CONTROL: Self = Self(40);

    /// End the machine's power state — power it off, or reset it — through
    /// the `system_power` syscall (`abi-v1` number 105).
    ///
    /// This is the widest-blast-radius authority in the system: it stops
    /// every task of every principal at once, on every seat, whether or not
    /// the holder owns them. It guards a whole class of authority — the
    /// machine's power state itself, not one device or one process — and no
    /// existing capability expresses it at that granularity:
    /// `CAP_PROC_CONTROL` reaches other principals' *processes* but never
    /// the platform, `CAP_SEAT_ADMIN` administers seats on a machine that
    /// stays running, and `CAP_DRV_KERNEL` loads code rather than ending
    /// execution. Owning the console, holding a seat lease, or running as
    /// the system user grants nothing here — there is no ambient path.
    ///
    /// It is granted in the administrative ceiling only, so an ordinary
    /// account's desktop renders its power actions with the Authority Mark
    /// and never attempts them. The kernel checks it at dispatch, before the
    /// handler touches any state, and audits every call — allowed or
    /// refused.
    pub const SYSTEM_POWER: Self = Self(41);

    /// Compose raw storage devices, and destroy the on-disk metadata that
    /// says how they are composed — creating a RAID array over blank disks,
    /// admitting a device into a live array, retiring a member from one, and
    /// stopping an array.
    ///
    /// These acts overwrite disks and change what a mounted filesystem is
    /// actually made of, so they are a guarded class rather than something a
    /// principal who merely reaches storage may do. The class is the whole
    /// composition surface — every device the composer can reach and every
    /// array it serves, not one disk or one operation — and no existing
    /// capability expresses it at that granularity: `CAP_FS_MOUNT` publishes
    /// and retracts a *volume* on a device someone else composed and is held
    /// by every principal who may mount removable media, so reusing it would
    /// hand anyone who can mount a memory stick the authority to overwrite
    /// every blank disk in the machine; `CAP_FS_ACCESS` is the coarse "may
    /// use the filesystem at all" gate; `CAP_HW_EMIT` publishes a discovered
    /// node and writes nothing to a medium.
    ///
    /// Reading array and member state needs nothing from here — that is the
    /// System Information API's own gate. This capability guards only the
    /// mutations, which the array composer checks against the caller's
    /// kernel-attested origin before it touches a device, refusing and
    /// auditing otherwise.
    pub const STORAGE_ADMIN: Self = Self(42);

    /// Spawn a **canonical parser sandbox** — and nothing else.
    ///
    /// Admits exactly one shape of `spawn`: a child the kernel itself brands
    /// capability-empty, with no credential switch and no console inherit.
    /// Every other spawn still needs [`PROC_SPAWN`](Self::PROC_SPAWN), which
    /// subsumes this one — a principal that may start *any* process may
    /// obviously start a restricted one.
    ///
    /// It exists so a principal that must decode untrusted input in an
    /// isolated worker — the graphical login screen rasterising the shipped
    /// wallpaper — need not hold the far broader authority to start a general
    /// process. The handler checks it only once it has decoded the attach
    /// block and knows the request is that canonical shape; a caller holding
    /// neither capability is refused before the block is even staged.
    pub const SANDBOX_SPAWN: Self = Self(43);

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
        (Self::IPC_ENDPOINT, "CAP_IPC_ENDPOINT"),
        (Self::SHM, "CAP_SHM"),
        (Self::FS_ACCESS, "CAP_FS_ACCESS"),
        (Self::SPAWN_AS_USER, "CAP_SPAWN_AS_USER"),
        (Self::SYSINFO_INTROSPECT, "CAP_SYSINFO_INTROSPECT"),
        (Self::SEAT_ADMIN, "CAP_SEAT_ADMIN"),
        (Self::MEM_PIN, "CAP_MEM_PIN"),
        (Self::NET_ADMIN, "CAP_NET_ADMIN"),
        (Self::NET, "CAP_NET"),
        (Self::SCHED_REALTIME, "CAP_SCHED_REALTIME"),
        (Self::NET_BIND_PRIVILEGED, "CAP_NET_BIND_PRIVILEGED"),
        (Self::FS_CHOWN, "CAP_FS_CHOWN"),
        (Self::PROC_CONTROL, "CAP_PROC_CONTROL"),
        (Self::SYSTEM_POWER, "CAP_SYSTEM_POWER"),
        (Self::STORAGE_ADMIN, "CAP_STORAGE_ADMIN"),
        (Self::SANDBOX_SPAWN, "CAP_SANDBOX_SPAWN"),
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
        assert_eq!(CapabilityId::IPC_ENDPOINT.as_u16(), 28);
        assert_eq!(CapabilityId::SHM.as_u16(), 29);
        assert_eq!(CapabilityId::FS_ACCESS.as_u16(), 30);
        assert_eq!(CapabilityId::SPAWN_AS_USER.as_u16(), 31);
        assert_eq!(CapabilityId::SYSINFO_INTROSPECT.as_u16(), 32);
        assert_eq!(CapabilityId::SEAT_ADMIN.as_u16(), 33);
        assert_eq!(CapabilityId::MEM_PIN.as_u16(), 34);
        assert_eq!(CapabilityId::NET_ADMIN.as_u16(), 35);
        assert_eq!(CapabilityId::NET.as_u16(), 36);
        assert_eq!(CapabilityId::SCHED_REALTIME.as_u16(), 37);
        assert_eq!(CapabilityId::NET_BIND_PRIVILEGED.as_u16(), 38);
        assert_eq!(CapabilityId::FS_CHOWN.as_u16(), 39);
        assert_eq!(CapabilityId::PROC_CONTROL.as_u16(), 40);
        assert_eq!(CapabilityId::SYSTEM_POWER.as_u16(), 41);
        assert_eq!(CapabilityId::STORAGE_ADMIN.as_u16(), 42);
        assert_eq!(CapabilityId::SANDBOX_SPAWN.as_u16(), 43);
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
        assert_eq!(CapabilityId::SEAT_ADMIN.name(), Some("CAP_SEAT_ADMIN"));
        assert_eq!(CapabilityId::MEM_PIN.name(), Some("CAP_MEM_PIN"));
        assert_eq!(CapabilityId::PROC_CONTROL.name(), Some("CAP_PROC_CONTROL"));
        assert_eq!(CapabilityId::SYSTEM_POWER.name(), Some("CAP_SYSTEM_POWER"));
        assert_eq!(
            CapabilityId::SANDBOX_SPAWN.name(),
            Some("CAP_SANDBOX_SPAWN")
        );

        // Every named id round-trips name -> id -> name.
        for &(cap, name) in CapabilityId::NAMED {
            assert_eq!(cap.name(), Some(name));
            assert_eq!(CapabilityId::from_name(name), Some(cap));
        }
    }

    #[test]
    fn every_assigned_id_has_a_name() {
        // Capabilities 1..=43 are assigned in abi-v1; each must carry a
        // canonical name so `getcap`/`setcap` can render and accept it.
        for raw in 1..=43 {
            let cap = CapabilityId::from_raw(raw).expect("in range");
            assert!(cap.name().is_some(), "capability {raw} has no name");
        }
        // …and the assigned range stops there: the next id is free, so a new
        // capability cannot silently reuse one.
        assert_eq!(CapabilityId::from_raw(44).expect("in range").name(), None);
    }

    #[test]
    fn assigned_ids_are_unique() {
        for (i, &(cap, name)) in CapabilityId::NAMED.iter().enumerate() {
            for &(other, other_name) in &CapabilityId::NAMED[i + 1..] {
                assert_ne!(cap, other, "{name} and {other_name} share an id");
                assert_ne!(name, other_name, "{name} is spelled twice");
            }
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
