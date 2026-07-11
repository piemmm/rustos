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
//!
//! Driver-class (`CAP_MEM_DMA`, `CAP_IRQ_BIND`, …) and service-class
//! (`CAP_SPAWN_AS_USER`, `CAP_USERS_READ`, …) capabilities are never part
//! of an account ceiling: they belong to the specific system program whose
//! manifest requests them. An administrator administers the system; they
//! do not impersonate its services.

use rustos_abi::CapabilityId;
use rustos_caps::CapabilitySet;

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
];

/// The administrative set: the grants an administrator account carries on
/// top of [`SESSION_BASELINE`].
///
/// * `CAP_USER_ADMIN` — create/modify/delete/lock accounts and edit
///   grants.
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
pub const ADMINISTRATIVE_SET: &[CapabilityId] = &[
    CapabilityId::USER_ADMIN,
    CapabilityId::FS_MOUNT,
    CapabilityId::RLIMIT_RAISE,
    CapabilityId::AUDIT_READ,
    CapabilityId::SYSINFO_GLOBAL,
    CapabilityId::SYSINFO_KERNEL,
    CapabilityId::SYSINFO_HW,
    CapabilityId::TIME_SET,
    CapabilityId::TIME_HIRES,
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
    let mut caps = CapabilitySet::empty();
    for cap in SESSION_BASELINE {
        caps.insert(*cap);
    }
    caps
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
        assert_eq!(set.len(), 7);
        for cap in [
            CapabilityId::FS_ACCESS,
            CapabilityId::PROC_SPAWN,
            CapabilityId::CONSOLE_WRITE,
            CapabilityId::CONSOLE_READ,
            CapabilityId::DISPLAY,
            CapabilityId::INPUT_READ,
            CapabilityId::SHM,
        ] {
            assert!(set.contains(cap), "{cap:?} missing from the baseline");
        }
    }

    #[test]
    fn administrator_ceiling_is_pinned() {
        let set = administrator_ceiling();
        assert_eq!(set.len(), 16);
        for cap in SESSION_BASELINE {
            assert!(set.contains(*cap), "{cap:?} missing from the ceiling");
        }
        for cap in [
            CapabilityId::USER_ADMIN,
            CapabilityId::FS_MOUNT,
            CapabilityId::RLIMIT_RAISE,
            CapabilityId::AUDIT_READ,
            CapabilityId::SYSINFO_GLOBAL,
            CapabilityId::SYSINFO_KERNEL,
            CapabilityId::SYSINFO_HW,
            CapabilityId::TIME_SET,
            CapabilityId::TIME_HIRES,
        ] {
            assert!(set.contains(cap), "{cap:?} missing from the ceiling");
        }
    }

    /// No service- or driver-class capability ever enters an account
    /// ceiling: the administrator administers the system, never
    /// impersonates its services or drivers.
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
