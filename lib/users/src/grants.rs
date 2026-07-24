//! The standard account capability-grant sets
//! (`plans/CAPABILITY_USE.md` §4.2, §4.3).
//!
//! An account's grant ceiling is authored into its
//! `/System/Security/Users` record by the image builder (`tools/mkimage`),
//! the installer, and — later — a `CAP_USER_ADMIN` holder. The two sets
//! every author composes from are policy, not per-author choice, so they
//! are defined here once beside the record format that stores them and
//! imported everywhere (the image builder's debug profile, the disk-image
//! test fixtures, and the kernel's session-program manifest), never
//! copy-pasted.
//!
//! * [`SESSION_BASELINE`] — what every interactive account is granted so
//!   an ordinary session works at all.
//! * [`ADMINISTRATIVE_SET`] — the additional grants that make an account
//!   an administrator. There is no admin flag, no wheel group, and no
//!   special uid: an administrator is exactly an account whose ceiling
//!   carries these capabilities.
//! * [`administrator_ceiling`] — the union of the two, the ceiling an
//!   administrator account (the debug image's `root`, the installer's
//!   first user) is seeded with.
//! * The per-service ceilings ([`DEVMGR_CEILING`], [`SYSINFOD_CEILING`],
//!   [`SEATMGR_CEILING`], [`LOGIN_CEILING`]) — each service account's
//!   grant ceiling holds exactly its own service's needs, so the
//!   ceiling∩manifest intersection does real work: a compromised service
//!   cannot borrow a sibling's authority even if its manifest lied
//!   (`plans/USERS.md`).
//!
//! Driver-class (`CAP_MEM_DMA`, `CAP_IRQ_BIND`, …) and service-class
//! (`CAP_SPAWN_AS_USER`, `CAP_USERS_READ`, …) capabilities are never part
//! of an *interactive* account ceiling: they belong to the specific system
//! program whose manifest requests them — and, through that service's own
//! no-login account, to its dedicated per-service ceiling. An
//! administrator administers the system; they do not impersonate its
//! services.

use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;

/// The session baseline: the class capabilities every interactive
/// account's ceiling must include for an ordinary session to work.
///
/// * `CAP_FS_ACCESS` — "may use the filesystem at all"; real reach stays
///   per-inode, so a baseline holder still cannot write `/System`.
/// * `CAP_PROC_SPAWN` — "may run programs at all"; the child is bounded by
///   its *own* manifest intersected with this same ceiling.
/// * `CAP_CONSOLE_WRITE` / `CAP_CONSOLE_READ` — an interactive session's
///   inherited standard streams are console-backed; the fine authority
///   stays the inherited descriptor table.
/// * `CAP_DISPLAY` / `CAP_INPUT_READ` / `CAP_SHM` — the graphical session
///   class (`plans/CAPABILITY_USE.md` §4.6): acquiring a seat's exclusive,
///   revocable display lease, draining the *owned* seat's input channels,
///   and creating/granting the zero-copy frame region the display service
///   maps. The class capability only admits the syscall; the kernel still
///   owner-gates every acquire, drain, and present against the live lease
///   and every region against its owner, so a baseline holder gains no
///   reach into another session's seat or memory. Granted in the baseline
///   because a graphical login is an ordinary session, not an
///   administrative act; on a headless build there is no seat to acquire
///   and the grants are inert.
///
/// * `CAP_NET` — ordinary network use: opening datagram sockets and
///   originating/receiving transport traffic through the `netstack`
///   socket surface (`plans/NETWORK.md` §0). Baseline because using the
///   network is an ordinary part of an interactive session, not an
///   administrative act; the coarser `CAP_NET_ADMIN` (reconfiguring
///   interfaces) and `CAP_NET_RAW` (unmediated raw frames) are not
///   baseline. A program still only receives it if its own manifest
///   requests it, intersected with this ceiling.
///
/// Nothing else is baseline: self-scoped `sysinfo` queries, `stream_*` on
/// inherited descriptors, lowering one's own resource limits, and
/// `fs_getcwd` already require no capability. A sandboxed process still
/// gets none of this, because its manifest requests none of it.
///
/// This is an **account ceiling**, never any program's manifest: each
/// program requests exactly its own exercised set (the shell's is the
/// kernel's `SHELL_MANIFEST`, which stays strictly within this ceiling;
/// the desktop session's is the graphical class), and the intersection
/// with this ceiling does the security work.
pub const SESSION_BASELINE: &[CapabilityId] = &[
    CapabilityId::FS_ACCESS,
    CapabilityId::PROC_SPAWN,
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::DISPLAY,
    CapabilityId::INPUT_READ,
    CapabilityId::SHM,
    CapabilityId::NET,
];

/// The administrative set: the grants an administrator account carries on
/// top of [`SESSION_BASELINE`].
///
/// * `CAP_USER_ADMIN` — create/modify/delete/lock accounts and edit
///   grants.
/// * `CAP_FS_CHOWN` — reassign the owning user of any filesystem node
///   (the `chown(2)` privilege): administering who owns files is an
///   administrative act, not an ordinary session's, so it is granted here
///   rather than in the baseline. An ordinary owner can still set their
///   own file's group to a group they belong to without it.
/// * `CAP_FS_MOUNT` — mount and unmount volumes.
/// * `CAP_RLIMIT_RAISE` — raise hard resource limits above an inherited
///   ceiling.
/// * `CAP_AUDIT_READ` — read the hash-chained security audit log.
/// * `CAP_SYSINFO_GLOBAL` / `CAP_SYSINFO_KERNEL` / `CAP_SYSINFO_HW` —
///   system-wide observability (all processes, kernel memory statistics,
///   the hardware tree).
/// * `CAP_TIME_SET` — adjust the wall clock.
/// * `CAP_TIME_HIRES` — the full-resolution monotonic clock
///   (diagnostics and profiling).
/// * `CAP_MEM_PIN` — exempt a process's anonymous memory from the swap
///   tiers (`mem_pin`, bounded by the `pinned-memory-bytes` limit): the
///   operator-diagnostics power the monitoring and load-generation tools
///   request in their manifests, grantable only through a ceiling that
///   carries it.
/// * `CAP_NET_ADMIN` — administer the network stack: interface, address,
///   and route mutation through the `netstack` admin surface
///   (`plans/NETWORK.md` §3).
/// * `CAP_NET_BIND_PRIVILEGED` — bind a listening socket to a well-known
///   (privileged) port below the privileged-port bound (`netstack`'s
///   `Bind` gate, the Unix `CAP_NET_BIND_SERVICE` model): running a
///   privileged network service is an administrative act, so an
///   administrator's ceiling may grant it to a program whose manifest
///   requests it. Ordinary transport use stays baseline `CAP_NET`.
/// * `CAP_NET_RAW` — unmediated raw network access: raw frames and the
///   ICMP/ICMPv6 echo socket the diagnostic `ping` tool opens (`netstack`
///   gates that socket on it). Reaching below the transport layer is an
///   administrative act — the Unix `CAP_NET_RAW`/setuid-`ping` model — so
///   an administrator's ceiling may grant it to a program whose manifest
///   requests it (`ping`), while ordinary transport use stays baseline
///   `CAP_NET`. It is the network stack service's defining capability
///   among the *service* ceilings; carrying it here widens only what an
///   *administrator account* may be granted, never any service's identity.
pub const ADMINISTRATIVE_SET: &[CapabilityId] = &[
    CapabilityId::USER_ADMIN,
    CapabilityId::FS_CHOWN,
    CapabilityId::FS_MOUNT,
    CapabilityId::RLIMIT_RAISE,
    CapabilityId::AUDIT_READ,
    CapabilityId::SYSINFO_GLOBAL,
    CapabilityId::SYSINFO_KERNEL,
    CapabilityId::SYSINFO_HW,
    CapabilityId::TIME_SET,
    CapabilityId::TIME_HIRES,
    CapabilityId::MEM_PIN,
    CapabilityId::NET_ADMIN,
    CapabilityId::NET_BIND_PRIVILEGED,
    CapabilityId::NET_RAW,
];

/// The `devmgr` service account's grant ceiling: read the hardware tree,
/// load matched drivers, and bind an autoloaded NIC driver's device
/// channel into the network stack. `CAP_NET_ADMIN` is held only for that
/// last act — the `BindDriver` admin call the stack gates on it — never to
/// configure addresses or routes itself.
pub const DEVMGR_CEILING: &[CapabilityId] = &[
    CapabilityId::SYSINFO_HW,
    CapabilityId::DRV_LOAD,
    CapabilityId::NET_ADMIN,
    CapabilityId::LOG_EMIT,
];

/// The `sysinfod` service account's grant ceiling: introspect the kernel
/// for the System Information broker and serve its privileged endpoint.
pub const SYSINFOD_CEILING: &[CapabilityId] = &[
    CapabilityId::SYSINFO_INTROSPECT,
    CapabilityId::SYSINFO_HW,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The `netstack` service account's grant ceiling: drive the NIC frame
/// rings and serve the privileged network endpoint. `CAP_NET_RAW` also
/// lets it call a NIC driver's restricted-sender device channel (the
/// kernel gates that endpoint on `CAP_NET_RAW`); `CAP_SHM` lets it own the
/// shared frame-ring region it creates and grants to the driver. It
/// deliberately does **not** carry `CAP_NET_ADMIN` — the service
/// *enforces* that capability against its callers; it never needs to hold
/// it.
pub const NETSTACK_CEILING: &[CapabilityId] = &[
    CapabilityId::NET_RAW,
    CapabilityId::SHM,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The `seatmgr` service account's grant ceiling: administer seats and
/// serve the privileged seat endpoint.
pub const SEATMGR_CEILING: &[CapabilityId] = &[
    CapabilityId::SEAT_ADMIN,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The `login` service account's grant ceiling: run the prompt on the
/// console, read the user database, and drop the authenticated session
/// into the target account — the instructive shape: it holds
/// `CAP_SPAWN_AS_USER` while itself being an unprivileged no-login
/// service account (authority from ceiling∩manifest, never identity).
pub const LOGIN_CEILING: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::PROC_SPAWN,
    CapabilityId::USERS_READ,
    CapabilityId::SPAWN_AS_USER,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
    CapabilityId::SYSINFO_KERNEL,
    CapabilityId::FS_ACCESS,
];

/// The administrator account ceiling: [`SESSION_BASELINE`] ∪
/// [`ADMINISTRATIVE_SET`].
///
/// This is the grant the debug image's seeded `root` account and the
/// installer's first user carry. A program the account runs still receives
/// only its own manifest request intersected with this ceiling — the
/// ceiling widens what an account *may* be granted, never what any one
/// program gets.
#[must_use]
pub fn administrator_ceiling() -> CapabilitySet {
    let mut caps = session_baseline();
    for cap in ADMINISTRATIVE_SET {
        caps.insert(*cap);
    }
    caps
}

/// [`SESSION_BASELINE`] as a [`CapabilitySet`] — the ceiling an ordinary
/// (non-administrator) interactive account is seeded with.
#[must_use]
pub fn session_baseline() -> CapabilitySet {
    capability_set(SESSION_BASELINE)
}

/// Collect a grant list into a [`CapabilitySet`] — how a seeded account's
/// ceiling (a per-service ceiling above, or [`SESSION_BASELINE`]) becomes
/// the set its [`crate::UserRecord`] stores.
#[must_use]
pub fn capability_set(caps: &[CapabilityId]) -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    for cap in caps {
        set.insert(*cap);
    }
    set
}

#[cfg(test)]
mod tests {
    //! Pinning tests: the exact membership of each set, so widening or
    //! narrowing account policy is a reviewed test diff, never an
    //! accident.

    use super::*;

    #[test]
    fn session_baseline_is_pinned() {
        let set = session_baseline();
        assert_eq!(set.len(), 8);
        for cap in [
            CapabilityId::FS_ACCESS,
            CapabilityId::PROC_SPAWN,
            CapabilityId::CONSOLE_WRITE,
            CapabilityId::CONSOLE_READ,
            CapabilityId::DISPLAY,
            CapabilityId::INPUT_READ,
            CapabilityId::SHM,
            CapabilityId::NET,
        ] {
            assert!(set.contains(cap), "{cap:?} missing from the baseline");
        }
    }

    #[test]
    fn administrator_ceiling_is_pinned() {
        let set = administrator_ceiling();
        assert_eq!(set.len(), 22);
        for cap in SESSION_BASELINE {
            assert!(set.contains(*cap), "{cap:?} missing from the ceiling");
        }
        for cap in [
            CapabilityId::USER_ADMIN,
            CapabilityId::FS_CHOWN,
            CapabilityId::FS_MOUNT,
            CapabilityId::RLIMIT_RAISE,
            CapabilityId::AUDIT_READ,
            CapabilityId::SYSINFO_GLOBAL,
            CapabilityId::SYSINFO_KERNEL,
            CapabilityId::SYSINFO_HW,
            CapabilityId::TIME_SET,
            CapabilityId::TIME_HIRES,
            CapabilityId::MEM_PIN,
            CapabilityId::NET_ADMIN,
            CapabilityId::NET_BIND_PRIVILEGED,
            CapabilityId::NET_RAW,
        ] {
            assert!(set.contains(cap), "{cap:?} missing from the ceiling");
        }
    }

    /// Each service ceiling is exactly its service's needs — pinned, so
    /// widening a service's authority is a reviewed test diff, and no
    /// service ceiling contains a sibling's defining capability.
    #[test]
    fn service_ceilings_are_pinned_and_disjoint_in_authority() {
        assert_eq!(DEVMGR_CEILING.len(), 4);
        assert_eq!(SYSINFOD_CEILING.len(), 4);
        assert_eq!(NETSTACK_CEILING.len(), 4);
        assert_eq!(SEATMGR_CEILING.len(), 3);
        assert_eq!(LOGIN_CEILING.len(), 9);
        let devmgr = capability_set(DEVMGR_CEILING);
        let sysinfod = capability_set(SYSINFOD_CEILING);
        let netstack = capability_set(NETSTACK_CEILING);
        let seatmgr = capability_set(SEATMGR_CEILING);
        let login = capability_set(LOGIN_CEILING);
        // The capability that defines each service stays that service's
        // alone.
        assert!(devmgr.contains(CapabilityId::DRV_LOAD));
        for other in [&sysinfod, &netstack, &seatmgr, &login] {
            assert!(!other.contains(CapabilityId::DRV_LOAD));
        }
        assert!(sysinfod.contains(CapabilityId::SYSINFO_INTROSPECT));
        for other in [&devmgr, &netstack, &seatmgr, &login] {
            assert!(!other.contains(CapabilityId::SYSINFO_INTROSPECT));
        }
        assert!(netstack.contains(CapabilityId::NET_RAW));
        for other in [&devmgr, &sysinfod, &seatmgr, &login] {
            assert!(!other.contains(CapabilityId::NET_RAW));
        }
        assert!(seatmgr.contains(CapabilityId::SEAT_ADMIN));
        for other in [&devmgr, &sysinfod, &netstack, &login] {
            assert!(!other.contains(CapabilityId::SEAT_ADMIN));
        }
        assert!(login.contains(CapabilityId::SPAWN_AS_USER));
        assert!(login.contains(CapabilityId::USERS_READ));
        for other in [&devmgr, &sysinfod, &netstack, &seatmgr] {
            assert!(!other.contains(CapabilityId::SPAWN_AS_USER));
            assert!(!other.contains(CapabilityId::USERS_READ));
        }
    }

    /// No service- or driver-class capability ever enters an
    /// *interactive* account ceiling: the administrator administers the
    /// system, never impersonates its services or drivers.
    #[test]
    fn ceiling_excludes_service_and_driver_class_capabilities() {
        let set = administrator_ceiling();
        for cap in [
            CapabilityId::SPAWN_AS_USER,
            CapabilityId::USERS_READ,
            CapabilityId::SYSINFO_INTROSPECT,
            CapabilityId::INPUT_INJECT,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::MMIO_MAP,
            CapabilityId::HW_EMIT,
            CapabilityId::DRV_LOAD,
            CapabilityId::DRV_KERNEL,
        ] {
            assert!(!set.contains(cap), "{cap:?} must not be in a ceiling");
        }
    }
}
