//! The manifest-requested capability list of every embedded program the
//! runtime `spawn` syscall resolves, and of PID 1 `init`
//! (`plans/CAPABILITY_USE.md` CU2).
//!
//! Each list is a program's **request** — the manifest side of the
//! `user ceiling ∩ manifest request` intersection the admit path derives a
//! child's effective set from — sized to every gated syscall the program
//! has a code path to issue, **including capability-gated optional
//! features that degrade gracefully when the intersection strips them**
//! (`plans/CAPABILITY_USE.md` §4.5, CU7). Requesting wide is not a grant
//! (the account ceiling bounds every session process — that intersection
//! is the security boundary), but an *unexercised* request is unaudited
//! surface, so a capability no code path uses stays out. The manifest
//! describes what the code *can* do; the ceiling describes what the user
//! *may* do. The registry rows in `spawn_layout` consume these lists; the
//! unit tests below pin each one, so a manifest change is a reviewed diff,
//! never an accident.
//!
//! The lists are pure data, free of the baked `rxe` fixtures the registry
//! rows carry, so they compile — and their pinning tests run — on the CI
//! host under `cargo test` as well as on each freestanding production
//! build whose registry consumes them, and on no other configuration, so
//! they are never dead code.

use tairix_abi::CapabilityId;

/// The session baseline (`plans/CAPABILITY_USE.md` §4.2) — the ceiling
/// every interactive account is seeded with. Account policy shared with
/// the users-database authors (the image builder's seeded grants, the
/// installer), so its one definition lives beside the account record in
/// `lib/users` and is re-exported here for the manifest-sizing tests
/// below; the per-capability rationale lives on that definition. A
/// program's manifest is sized to its own code paths — the shell's is
/// [`SHELL_MANIFEST`], not this ceiling.
// Consumed only by the exact-set pinning tests on the CI host, which
// verify every session tool's request against this ceiling; a freestanding
// production build has no consumer, so the re-export is test-gated.
#[cfg(test)]
pub use tairix_users::SESSION_BASELINE;

/// The shell's manifest: the console pair for its REPL, `CAP_FS_ACCESS`
/// for `cd`/redirection/completion through the secured VFS, and
/// `CAP_PROC_SPAWN` to run programs. Exactly the capabilities the shell
/// has code paths for — deliberately **not** the account baseline
/// (`tairix_users::SESSION_BASELINE`), which additionally carries the
/// graphical-session class (`CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM`)
/// the shell never exercises: a manifest is sized to the program's code
/// paths, never to what an account may hold. Nothing else: `wait`,
/// `signal`, `rlimit_get`/`rlimit_set`, and `fs_getcwd` — the rest of
/// what the shell calls — are ungated.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SHELL_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
    CapabilityId::PROC_SPAWN,
];

/// The login service's manifest: the console pair for its prompt
/// (`stream_read`/`stream_write`/`stream_input_mode` over its inherited
/// per-console streams), `CAP_PROC_SPAWN` plus `CAP_SPAWN_AS_USER` for the
/// one privileged identity transition (starting the authenticated
/// account's shell under that account's kernel-resolved credential and
/// ceiling), `CAP_USERS_READ` to verify offered credentials against the
/// kernel-held user database (`users_db_read`/`users_db_wait`),
/// `CAP_IPC_BIND_PRIVILEGED` to bind its console's reserved elevation
/// rendezvous (the `elevate_endpoint` id the shell's `elevate` builtin
/// calls — reserved ids fail closed against squatters),
/// `CAP_LOG_EMIT` for its structured audit records,
/// `CAP_SYSINFO_KERNEL` so the full-screen view's bottom bar can show the
/// memory figures (a refusal degrades that one figure to a placeholder,
/// never the login), and `CAP_FS_ACCESS` for exactly one read-only probe
/// — whether the desktop-session bundle exists, which decides if the
/// graphical option is offered (`plans/DISPLAY.md` D7d). Credentials
/// still flow only through the gated `users_db_read` syscall, never the
/// filesystem.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const LOGIN_MANIFEST: &[CapabilityId] = &[
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

/// The device-manager service's manifest: `CAP_SYSINFO_HW` for the
/// privileged hardware-tree view (`hw_tree_read`/`hw_tree_wait`),
/// `CAP_DRV_LOAD` for the restricted driver-store `ipc_call` endpoint (the
/// kernel re-checks it at the load gate), `CAP_NET_ADMIN` to hand a bound
/// NIC driver's device channel to the network stack (the `BindDriver`
/// admin call the stack gates on it), `CAP_FS_ACCESS` to read the
/// world-readable machine-wide network policy post-unlock and deliver it
/// to the network stack on its behalf (the stack-wide `net.*` settings
/// and the per-interface `network.conf`, `plans/NETWORK.md` N9b-2 /
/// N9b-3-1; `netstack` itself holds no filesystem capability), and
/// `CAP_LOG_EMIT` for its structured diagnostics. It writes no standard
/// stream (no console pair) and holds no resource capability: the kernel
/// mints a loaded driver's grants from its matched node, never from this
/// caller.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const DEVMGR_MANIFEST: &[CapabilityId] = &[
    CapabilityId::SYSINFO_HW,
    CapabilityId::DRV_LOAD,
    CapabilityId::NET_ADMIN,
    CapabilityId::FS_ACCESS,
    CapabilityId::LOG_EMIT,
];

/// The System Information service's manifest: `CAP_SYSINFO_INTROSPECT`
/// (the privileged, unfiltered global introspection primitive it alone
/// holds and re-scopes per client), `CAP_SYSINFO_HW` for the hardware-tree
/// query (`hw_tree_read`), `CAP_IPC_BIND_PRIVILEGED` to bind the reserved
/// well-known `SYSINFO_ENDPOINT` rendezvous (reserved ids fail closed
/// against squatters serving forged system state), and `CAP_LOG_EMIT` for
/// its structured audit records. Per-query scoping stays in the broker
/// against each caller's kernel-attested origin.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SYSINFOD_MANIFEST: &[CapabilityId] = &[
    CapabilityId::SYSINFO_INTROSPECT,
    CapabilityId::SYSINFO_HW,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The seat-manager service's manifest (`plans/DISPLAY.md` D3):
/// `CAP_SEAT_ADMIN` — the seat-multiplexing authority this service is the
/// sole holder of, re-checked by the kernel on every `seat_switch` /
/// `seat_revoke` it forwards — `CAP_IPC_BIND_PRIVILEGED` to bind the
/// reserved well-known `SEATMGR_ENDPOINT` rendezvous (reserved ids fail
/// closed against squatters intercepting seat-administration requests),
/// and `CAP_LOG_EMIT` for its structured audit records. It writes no
/// standard stream (no console pair) and reads no filesystem.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SEATMGR_MANIFEST: &[CapabilityId] = &[
    CapabilityId::SEAT_ADMIN,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The `netstack` network-stack service's manifest (`plans/NETWORK.md` §3):
/// `CAP_NET_RAW` for the NIC frame rings it alone drives (and to call a NIC
/// driver's restricted-sender device channel), `CAP_SHM` to create and
/// grant the shared frame-ring region each channel client owns,
/// `CAP_IPC_BIND_PRIVILEGED` to bind the reserved `NETSTACK_ENDPOINT` and
/// `NETSTACK_SOCKET_ENDPOINT` rendezvous (reserved ids fail closed against
/// squatters serving forged network state), and `CAP_LOG_EMIT` for its
/// structured audit records. It deliberately does **not** request
/// `CAP_NET_ADMIN` — the service *enforces* that capability against its
/// callers; it never holds it. The effective set is this request
/// intersected with the `netstack` account's `NETSTACK_CEILING`, which
/// carries exactly the same four.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const NETSTACK_MANIFEST: &[CapabilityId] = &[
    CapabilityId::NET_RAW,
    CapabilityId::SHM,
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::LOG_EMIT,
];

/// The `fontd` font service's manifest (`plans/FONT-SERVICE.md`):
/// `CAP_IPC_BIND_PRIVILEGED` to bind the reserved well-known
/// `FONT_ENDPOINT` rendezvous (reserved ids fail closed against a squatter
/// feeding forged glyph coverage to the compositor), `CAP_FS_ACCESS` for the
/// startup scan of the `/System/Fonts` family manifests and the first-use read
/// of each face through the secured VFS (which still authorises every path
/// per-inode under the service's attested identity, and `/System` is mounted
/// read-only so this reach can never write), and `CAP_LOG_EMIT` for its audit
/// records. It requests no spawn or network authority, and the untrusted
/// TrueType parse runs in this service's own isolated address space, so a
/// malformed face can fault only this sandbox. Drawing text is not a security
/// boundary, so serving glyph coverage needs no capability of its own.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const FONTD_MANIFEST: &[CapabilityId] = &[
    CapabilityId::IPC_BIND_PRIVILEGED,
    CapabilityId::FS_ACCESS,
    CapabilityId::LOG_EMIT,
];

/// The `ps` tool's manifest: `CAP_CONSOLE_WRITE` for its listing on fd 1
/// and diagnostics on fd 2, `CAP_FS_ACCESS` because its short-help
/// switches read the bundle's own `Help/` tree through the secured VFS
/// (which still authorises every path per-inode under the caller's
/// attested identity), and `CAP_SYSINFO_GLOBAL` because `-e`/`-A` issue
/// the `GLOBAL_PROCESS_LIST` query — an optional feature above the
/// session baseline: armed only when the account ceiling carries the
/// capability (an administrator's intersection keeps it), refused with a
/// stated reason for everyone else while the self-scoped listing still
/// works. Every per-query scope is enforced by `sysinfod` against this
/// process's kernel-attested origin.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const PS_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::FS_ACCESS,
    CapabilityId::SYSINFO_GLOBAL,
];

/// The `sysinfo` tool's manifest: like `ps`, `CAP_CONSOLE_WRITE` for the
/// rendered results on fd 1 plus `CAP_FS_ACCESS` because its short-help
/// switches read the bundle's own `Help/` tree through the secured VFS
/// (which still authorises every path per-inode under the caller's
/// attested identity), plus the three privileged observability requests
/// its query surface exercises — `CAP_SYSINFO_GLOBAL`
/// (`GLOBAL_PROCESS_LIST`), `CAP_SYSINFO_KERNEL` (`KERNEL_MEMORY_STATS`),
/// and `CAP_SYSINFO_HW` (`HARDWARE_TREE`). Each is an optional feature
/// above the session baseline: armed only when the account ceiling
/// carries it (an administrator's intersection keeps all three), refused
/// with a stated reason for everyone else while the ungated queries still
/// work. Per-query authority stays with `sysinfod` and the caller's
/// attested origin.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SYSINFO_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::FS_ACCESS,
    CapabilityId::SYSINFO_GLOBAL,
    CapabilityId::SYSINFO_KERNEL,
    CapabilityId::SYSINFO_HW,
];

/// The `top` tool's manifest: `CAP_CONSOLE_WRITE` for its full-screen
/// display on fd 1, `CAP_CONSOLE_READ` for raw-mode keystrokes on fd 0
/// (the latter also authorises its `stream_input_mode` raw discipline),
/// `CAP_FS_ACCESS` because its short-help switches read the bundle's own
/// `Help/` tree through the secured VFS (which still authorises every
/// path per-inode under the caller's attested identity), plus the two
/// privileged observability requests its optional features exercise —
/// `CAP_SYSINFO_KERNEL` for the memory summary line
/// (`KERNEL_MEMORY_STATS`, every refresh) and `CAP_SYSINFO_GLOBAL` for
/// the `a` system-wide toggle (`GLOBAL_PROCESS_LIST`). Each is an
/// optional feature above the session baseline: armed only when the
/// account ceiling carries it (an administrator's intersection keeps
/// both), degraded to the stated placeholder/refusal for everyone else
/// while the self-scoped viewer keeps working. The `USER` column's uid →
/// name map comes from the ungated, secret-free user-directory `sysinfo`
/// query, so no further capability is requested; `terminal_size` is
/// ungated and per-query `sysinfo` scope is enforced by `sysinfod`.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const TOP_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
    CapabilityId::SYSINFO_GLOBAL,
    CapabilityId::SYSINFO_KERNEL,
];

/// The `sysmon` monitor's manifest: the `top` console/help surface
/// (`CAP_CONSOLE_WRITE` for the full-screen display on fd 1,
/// `CAP_CONSOLE_READ` for raw-mode keystrokes on fd 0 — also authorising
/// its `stream_input_mode` raw discipline — and `CAP_FS_ACCESS` for the
/// short-help read of its own bundle's `Help/` tree through the secured
/// VFS) plus the three privileged features its panels exercise:
/// `CAP_SYSINFO_KERNEL` for the kernel-wide statistics every refresh
/// issues (`KERNEL_MEMORY_STATS`, `MEMORY_PRESSURE`, `RECLAIM_STATS`,
/// `RAMZIP_STATS`, `CPU_LOAD`), `CAP_SYSINFO_GLOBAL` for the all-process
/// census, `CAP_SYSINFO_HW` for the interrupt-lines panel's `IRQ_LIST`
/// (which names which driver task owns each physical interrupt line —
/// cross-principal surface topology, gated like the hardware tree and
/// seat inventory), and `CAP_MEM_PIN` for the startup `mem_pin` that
/// exempts the monitor's own memory from the swap tiers it observes
/// (`plans/STRESSTEST.md` ST4). Each is an optional feature above the
/// session baseline: armed only when the account ceiling carries it (an
/// administrator's intersection keeps them all), degraded to the stated
/// per-panel refusal — or an unpinned title-line notice — for everyone
/// else while the session keeps running.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SYSMON_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
    CapabilityId::SYSINFO_GLOBAL,
    CapabilityId::SYSINFO_KERNEL,
    CapabilityId::SYSINFO_HW,
    CapabilityId::MEM_PIN,
];

/// The `stress` load generator's manifest (`plans/STRESSTEST.md` ST5):
/// `CAP_CONSOLE_WRITE` for the dispatch/summary lines on fd 1 and
/// diagnostics on fd 2, `CAP_FS_ACCESS` because the disk-touching workers
/// write beneath the run's scratch directory (the secured VFS still
/// authorises every path per-inode under the caller's attested identity),
/// `CAP_PROC_SPAWN` because the controller re-enters its own attested
/// binary as the workers through the `@self` token (and starts the
/// installed `sysmon` bundle under `--monitor`), and `CAP_MEM_PIN` for
/// the startup `mem_pin` that keeps the controller responsive under the
/// very pressure it creates. No `CAP_CONSOLE_READ`: the tool reads
/// nothing from fd 0 (`^C` arrives through the audited signal intake).
/// Loading the machine needs no privilege beyond the caller's own
/// resource limits — the limits are the defence.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const STRESS_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::FS_ACCESS,
    CapabilityId::PROC_SPAWN,
    CapabilityId::MEM_PIN,
];

/// The `ls` tool's manifest: `CAP_CONSOLE_WRITE` for the listing on fd 1
/// and diagnostics on fd 2, plus `CAP_FS_ACCESS` because inspecting paths
/// and reading directories *is* the tool's job — the secured VFS still
/// authorises every path per-inode under the caller's attested identity.
/// No `CAP_CONSOLE_READ`: the tool reads nothing from fd 0.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const LS_MANIFEST: &[CapabilityId] = &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

/// The `cat` tool's manifest: `CAP_CONSOLE_WRITE` for the concatenated
/// stream on fd 1 and diagnostics on fd 2, `CAP_CONSOLE_READ` because the
/// `-` operand (and the no-operand default) reads standard input on fd 0,
/// and `CAP_FS_ACCESS` because reading its file operands *is* the tool's
/// job — the secured VFS still authorises every path per-inode under the
/// caller's attested identity.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const CAT_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
];

/// The `man` help tool's manifest: `CAP_CONSOLE_WRITE` for the rendered
/// page on fd 1 (and diagnostics on fd 2), `CAP_CONSOLE_READ` for the
/// pager's keystrokes on fd 0 (also authorising its `stream_input_mode`
/// raw discipline, as in `top`), `CAP_FS_ACCESS` because reading a
/// bundle's `Help/` documents *is* the tool's job — the secured VFS still
/// authorises every path per-inode under the caller's attested identity —
/// and `CAP_PROC_SPAWN` to re-spawn its own binary as the parser-sandbox
/// render worker (`docs/src/security/sandbox.md`): the foreign document is
/// parsed there, never in `man`'s own process.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const MAN_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
    CapabilityId::PROC_SPAWN,
];

/// The `clear` tool's manifest: `CAP_CONSOLE_WRITE` for the clear sequence
/// on fd 1 and `CAP_FS_ACCESS` because its short-help switches read the
/// bundle's own `Help/` tree through the secured VFS (which still
/// authorises every path per-inode under the caller's attested identity).
/// No `CAP_CONSOLE_READ`: the tool reads no input and leaves the input
/// discipline alone.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const CLEAR_MANIFEST: &[CapabilityId] = &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

/// The `reset` tool's manifest: `CAP_CONSOLE_WRITE` for the restoration
/// sequence on fd 1, `CAP_CONSOLE_READ` because restoring the cooked input
/// discipline (`stream_input_mode`) belongs to the principal that reads
/// the console, and `CAP_FS_ACCESS` because its short-help switches read
/// the bundle's own `Help/` tree through the secured VFS (which still
/// authorises every path per-inode under the caller's attested identity).
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const RESET_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
];

/// The `users` account-administration tool's manifest: the console pair
/// for its interactive prompts (`stream_read`/`stream_write`/
/// `stream_input_mode` over its inherited streams — the secret discipline
/// around passwords) plus
/// `CAP_USER_ADMIN` for the `users_admin` syscall it exists to drive.
/// Deliberately **above** the session baseline: only an account whose
/// ceiling carries `CAP_USER_ADMIN` (an administrator, §4.3 of
/// `plans/CAPABILITY_USE.md`) ends up with a working tool — on any other
/// account the intersection strips the capability and every operation is
/// refused at dispatch. Accounts are edited through the gated syscall and
/// the salt is read through the unprivileged `sys:random` resource, never
/// the filesystem; `CAP_FS_ACCESS` exists solely so the short-help
/// switches can read the bundle's own `Help/` tree through the secured
/// VFS (which still authorises every path per-inode under the caller's
/// attested identity).
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const USERS_TOOL_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::USER_ADMIN,
    CapabilityId::FS_ACCESS,
];

/// PID 1 `init`'s manifest: `CAP_CONSOLE_WRITE` for its startup banner
/// (`stream_write`), `CAP_PROC_SPAWN` to launch the boot services and
/// the per-console login supervisors, `CAP_SPAWN_AS_USER` because
/// every service and session it launches is switched onto its own
/// compiled-in service account (`plans/USERS.md` — the startup config
/// names the account, the kernel resolves the credential from the
/// boot-installed identity table), and `CAP_LOG_EMIT` so its service
/// manager can record structured service-lifecycle events (skip, spawn,
/// ready) through the diagnostic sink — the same authority the boot
/// services it launches (`login`, `devmgr`, `sysinfod`) already carry.
/// As a system program its manifest is
/// also its ceiling (there is no account row for the system principal),
/// and each child it spawns is bounded by that child's *own* registered
/// manifest intersected with its service account's ceiling, never by
/// this set.
pub const INIT_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::PROC_SPAWN,
    CapabilityId::SPAWN_AS_USER,
    CapabilityId::LOG_EMIT,
];

#[cfg(test)]
mod tests {
    //! Pinning tests: one exact-set assertion per manifest, so widening or
    //! narrowing any program's request is a reviewed test diff, never an
    //! accident — plus the invariant that every session *tool* requests
    //! within the session baseline, so a baseline-only account can run the
    //! whole default toolset.

    use tairix_abi::ProgramKind;
    use tairix_caps::CapabilitySet;

    use super::*;

    /// The exact-set form of `caps`: duplicates collapse, order is
    /// irrelevant, and equality compares all 256 bits.
    fn set(caps: &[CapabilityId]) -> CapabilitySet {
        let mut out = CapabilitySet::empty();
        for cap in caps {
            out.insert(*cap);
        }
        out
    }

    #[test]
    fn shell_manifest_is_pinned() {
        // The shell requests exactly its exercised capabilities; the wider
        // account baseline (which now carries the graphical-session class)
        // is a ceiling, never this program's request.
        assert_eq!(
            set(SHELL_MANIFEST),
            set(&[
                CapabilityId::FS_ACCESS,
                CapabilityId::PROC_SPAWN,
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
            ])
        );
        // And stays strictly within the baseline, so under any account the
        // intersection keeps the shell's whole request.
        for cap in SHELL_MANIFEST {
            assert!(set(SESSION_BASELINE).contains(*cap));
        }
    }

    #[test]
    fn login_manifest_is_pinned() {
        assert_eq!(
            set(LOGIN_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::PROC_SPAWN,
                CapabilityId::USERS_READ,
                CapabilityId::SPAWN_AS_USER,
                CapabilityId::IPC_BIND_PRIVILEGED,
                CapabilityId::LOG_EMIT,
                CapabilityId::SYSINFO_KERNEL,
                CapabilityId::FS_ACCESS,
            ])
        );
    }

    #[test]
    fn devmgr_manifest_is_pinned() {
        assert_eq!(
            set(DEVMGR_MANIFEST),
            set(&[
                CapabilityId::SYSINFO_HW,
                CapabilityId::DRV_LOAD,
                CapabilityId::NET_ADMIN,
                CapabilityId::FS_ACCESS,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    #[test]
    fn sysinfod_manifest_is_pinned() {
        assert_eq!(
            set(SYSINFOD_MANIFEST),
            set(&[
                CapabilityId::SYSINFO_INTROSPECT,
                CapabilityId::SYSINFO_HW,
                CapabilityId::IPC_BIND_PRIVILEGED,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    #[test]
    fn seatmgr_manifest_is_pinned() {
        assert_eq!(
            set(SEATMGR_MANIFEST),
            set(&[
                CapabilityId::SEAT_ADMIN,
                CapabilityId::IPC_BIND_PRIVILEGED,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    #[test]
    fn netstack_manifest_is_pinned() {
        assert_eq!(
            set(NETSTACK_MANIFEST),
            set(&[
                CapabilityId::NET_RAW,
                CapabilityId::SHM,
                CapabilityId::IPC_BIND_PRIVILEGED,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    #[test]
    fn fontd_manifest_is_pinned() {
        assert_eq!(
            set(FONTD_MANIFEST),
            set(&[
                CapabilityId::IPC_BIND_PRIVILEGED,
                CapabilityId::FS_ACCESS,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    #[test]
    fn ps_manifest_is_pinned() {
        assert_eq!(
            set(PS_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::FS_ACCESS,
                CapabilityId::SYSINFO_GLOBAL,
            ])
        );
    }

    #[test]
    fn sysinfo_manifest_is_pinned() {
        assert_eq!(
            set(SYSINFO_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::FS_ACCESS,
                CapabilityId::SYSINFO_GLOBAL,
                CapabilityId::SYSINFO_KERNEL,
                CapabilityId::SYSINFO_HW,
            ])
        );
    }

    #[test]
    fn top_manifest_is_pinned() {
        assert_eq!(
            set(TOP_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::FS_ACCESS,
                CapabilityId::SYSINFO_GLOBAL,
                CapabilityId::SYSINFO_KERNEL,
            ])
        );
    }

    #[test]
    fn sysmon_manifest_is_pinned() {
        assert_eq!(
            set(SYSMON_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::FS_ACCESS,
                CapabilityId::SYSINFO_GLOBAL,
                CapabilityId::SYSINFO_KERNEL,
                CapabilityId::SYSINFO_HW,
                CapabilityId::MEM_PIN,
            ])
        );
    }

    #[test]
    fn stress_manifest_is_pinned() {
        assert_eq!(
            set(STRESS_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::FS_ACCESS,
                CapabilityId::PROC_SPAWN,
                CapabilityId::MEM_PIN,
            ])
        );
    }

    #[test]
    fn ls_manifest_is_pinned() {
        assert_eq!(
            set(LS_MANIFEST),
            set(&[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS])
        );
    }

    #[test]
    fn cat_manifest_is_pinned() {
        assert_eq!(
            set(CAT_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::FS_ACCESS,
            ])
        );
    }

    #[test]
    fn clear_manifest_is_pinned() {
        assert_eq!(
            set(CLEAR_MANIFEST),
            set(&[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS])
        );
    }

    #[test]
    fn reset_manifest_is_pinned() {
        assert_eq!(
            set(RESET_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::FS_ACCESS,
            ])
        );
    }

    #[test]
    fn man_manifest_is_pinned() {
        assert_eq!(
            set(MAN_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::FS_ACCESS,
                CapabilityId::PROC_SPAWN,
            ])
        );
    }

    #[test]
    fn users_tool_manifest_is_pinned() {
        assert_eq!(
            set(USERS_TOOL_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
                CapabilityId::USER_ADMIN,
                CapabilityId::FS_ACCESS,
            ])
        );
    }

    #[test]
    fn init_manifest_is_pinned() {
        assert_eq!(
            set(INIT_MANIFEST),
            set(&[
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::PROC_SPAWN,
                CapabilityId::SPAWN_AS_USER,
                CapabilityId::LOG_EMIT,
            ])
        );
    }

    /// Every capability a session tool requests is either in the session
    /// baseline (its core function, so a baseline-only account can run
    /// the whole default toolset usefully) or in the administrator
    /// ceiling (an optional, gracefully-degrading feature that arms only
    /// for an entitled account's intersection). A session tool never
    /// requests a service- or driver-class capability: those belong to
    /// the specific system program whose manifest requests them.
    #[test]
    fn session_tool_requests_stay_within_the_administrator_ceiling() {
        let ceiling = tairix_users::administrator_ceiling();
        for manifest in [
            CAT_MANIFEST,
            CLEAR_MANIFEST,
            DESKTOP_SESSION_REQUEST,
            LS_MANIFEST,
            MAN_MANIFEST,
            PS_MANIFEST,
            RESET_MANIFEST,
            SYSINFO_MANIFEST,
            SYSMON_MANIFEST,
            TOP_MANIFEST,
        ] {
            for cap in manifest {
                assert!(ceiling.contains(*cap), "{cap:?} exceeds the admin ceiling");
            }
        }
    }

    /// The exact above-baseline subset of every session tool's request —
    /// each entry names an optional, capability-gated feature that
    /// degrades gracefully when a non-administrator's intersection strips
    /// it (the tool still performs its core function within the
    /// baseline). Widening any tool above the baseline is a reviewed diff
    /// here, never an accident; a tool absent from this list requests
    /// nothing above the baseline.
    #[test]
    fn session_tool_requests_above_the_baseline_are_the_audited_set() {
        let baseline = set(SESSION_BASELINE);
        let above = |manifest: &[CapabilityId]| {
            let mut out = CapabilitySet::empty();
            for cap in manifest {
                if !baseline.contains(*cap) {
                    out.insert(*cap);
                }
            }
            out
        };
        // ps: `-e`/`-A` list every process.
        assert_eq!(above(PS_MANIFEST), set(&[CapabilityId::SYSINFO_GLOBAL]));
        // top: the memory summary line and the `a` system-wide toggle.
        assert_eq!(
            above(TOP_MANIFEST),
            set(&[CapabilityId::SYSINFO_GLOBAL, CapabilityId::SYSINFO_KERNEL])
        );
        // sysmon: the kernel-wide statistics panels, the all-process
        // census, the interrupt-lines panel, and the startup self-pin.
        assert_eq!(
            above(SYSMON_MANIFEST),
            set(&[
                CapabilityId::SYSINFO_GLOBAL,
                CapabilityId::SYSINFO_KERNEL,
                CapabilityId::SYSINFO_HW,
                CapabilityId::MEM_PIN,
            ])
        );
        // sysinfo: the global process, kernel-memory, and hardware-tree
        // queries of its reporting surface.
        assert_eq!(
            above(SYSINFO_MANIFEST),
            set(&[
                CapabilityId::SYSINFO_GLOBAL,
                CapabilityId::SYSINFO_KERNEL,
                CapabilityId::SYSINFO_HW,
            ])
        );
        // Every other session tool — including the desktop session, whose
        // whole graphical class is baseline (CU6) — requests nothing above
        // the baseline.
        for manifest in [
            CAT_MANIFEST,
            CLEAR_MANIFEST,
            DESKTOP_SESSION_REQUEST,
            LS_MANIFEST,
            MAN_MANIFEST,
            RESET_MANIFEST,
        ] {
            assert_eq!(above(manifest), CapabilitySet::empty());
        }
    }

    /// The store-only account-administration tools' expected request:
    /// console write for their output, the administrative gate the
    /// `users_admin` syscall demands, and the filesystem gate their
    /// short-help read needs. No console-read: they never prompt. They
    /// ship purely as discovered on-disk bundles — the boot floor never
    /// grows — so no `spawn_layout` row or manifest constant exists for
    /// them and their `AppInfo.toml` is pinned here directly.
    const ADMIN_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::USER_ADMIN,
        CapabilityId::FS_ACCESS,
    ];

    /// The account-administration tools request the administrative gate,
    /// their console reach, and the filesystem gate their short-help read
    /// needs — nothing else — and every capability each requests is within
    /// the administrator ceiling, so an administrator's intersection loses
    /// nothing.
    #[test]
    fn admin_tool_requests_are_within_the_administrator_ceiling() {
        let ceiling = tairix_users::administrator_ceiling();
        for manifest in [USERS_TOOL_MANIFEST, ADMIN_TOOL_REQUEST] {
            for cap in manifest {
                assert!(ceiling.contains(*cap), "{cap:?} exceeds the admin ceiling");
            }
        }
    }

    // The store-only file tools' expected request: the console pair for
    // their prompts/diagnostics plus filesystem reach, which *is* their
    // job. They ship purely as discovered on-disk bundles — the boot
    // floor never grows — so no `spawn_layout` row or manifest constant
    // exists for them and their `AppInfo.toml` is pinned here directly.
    const FILE_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::CONSOLE_READ,
        CapabilityId::FS_ACCESS,
    ];

    // A file tool that additionally re-spawns its own binary as its
    // parser-sandbox worker (fstree's disassembly viewer decodes
    // every container and instruction window there, never in-process):
    // the file-tool request plus `CAP_PROC_SPAWN`.
    const SANDBOXED_FILE_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::CONSOLE_READ,
        CapabilityId::FS_ACCESS,
        CapabilityId::PROC_SPAWN,
    ];

    // The store-only pure text tools' expected request: console write
    // for their output and the filesystem gate their short-help read
    // needs — they touch no operand path and never prompt. They ship
    // purely as discovered on-disk bundles — the boot floor never
    // grows — so no `spawn_layout` row or manifest constant exists for
    // them and their `AppInfo.toml` is pinned here directly.
    const PURE_TOOL_REQUEST: &[CapabilityId] =
        &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

    // The `desktop` command app — the graphical desktop session
    // (plans/DISPLAY.md D7c, plans/APPWIN.md AW3/AW5), started by a
    // graphical login or by the shell's `desktop` command word: the
    // boot seat's revocable lease (CAP_DISPLAY), the owner-gated seat
    // input drains (CAP_INPUT_READ), the zero-copy frame regions it
    // creates for the display service and maps from each served app
    // window (CAP_SHM), the taskbar's launchers and program-library
    // popup (CAP_PROC_SPAWN), the trusted file picker and the catalog
    // stores (CAP_FS_ACCESS — the session lists directories, reads the
    // program-library documents, and opens the user's chosen file under
    // its own identity, then delegates that one file one-shot over
    // fd_grant; plans/CAPABILITY_USE.md CU6), and the command surface
    // (CAP_CONSOLE_WRITE — the short help on stdout and the fail-loud
    // teardown reasons on stderr). Binding the seat-scoped window
    // rendezvous needs no capability: the kernel authorises it by the
    // session's live seat lease. It ships purely as a discovered
    // on-disk bundle — the boot floor never grows — so no
    // `spawn_layout` row or manifest constant exists for it and its
    // `AppInfo.toml` is pinned here directly.
    const DESKTOP_SESSION_REQUEST: &[CapabilityId] = &[
        CapabilityId::DISPLAY,
        CapabilityId::INPUT_READ,
        CapabilityId::SHM,
        CapabilityId::PROC_SPAWN,
        CapabilityId::FS_ACCESS,
        CapabilityId::CONSOLE_WRITE,
    ];

    // The RAID array administration tool `mdadm` (plans/FIX-IO.md IO6f):
    // the pure-tool request, plus `CAP_SYSINFO_HW` for the array and
    // member reads (the composer gates its own read at the bar the
    // hardware tree is read under, so a caller cannot side-step the
    // System Information query by asking it directly), plus
    // `CAP_STORAGE_ADMIN` for the create/add/remove/stop mutations, which
    // overwrite disks and change what a mounted filesystem is made of.
    // The composer attests the caller kernel-side and refuses without the
    // capability; the tool only reports that refusal. Not an embedded
    // spawn-floor program, so the list lives only in this pin.
    const RAID_ADMIN_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::SYSINFO_HW,
        CapabilityId::STORAGE_ADMIN,
    ];

    // The device-inventory listing tools `lspci` and `lsusb`
    // (plans/DEVICES.md DEVICE1): the pure-tool request plus
    // `CAP_SYSINFO_HW` for the `HARDWARE_TREE` query they render. Not
    // embedded spawn-floor programs, so the list lives only in this pin.
    const HW_LIST_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::SYSINFO_HW,
    ];

    // The windowed file browser `files` (plans/APPWIN.md AW3): console
    // write for its fail-loud diagnostics, filesystem reach for the
    // listings it browses, CAP_SHM to create and grant the zero-copy
    // window frame region the desktop session maps, and CAP_PROC_SPAWN
    // to launch an activated <Name>.app bundle through the ordinary
    // signed app-load gate (plans/NEW-FILEMANAGER.md FM6b — double-click
    // / Enter on a bundle spawns its own Run, never a private path).
    // Not an embedded spawn-floor program, so the list lives only in
    // this pin.
    const FILES_BROWSER_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::SHM,
        CapabilityId::PROC_SPAWN,
    ];

    // The windowed terminal `terminal` (plans/APPWIN.md AW4,
    // plans/GUI-TERMINAL.md): console write for its fail-loud diagnostics,
    // CAP_PROC_SPAWN to host the user's shell as its own child over a pty,
    // CAP_SHM to create and grant the zero-copy window frame region the
    // desktop session maps, and CAP_FS_ACCESS to read and rewrite the
    // launching user's own terminal profile under their home — an ordinary
    // per-user store, reaching nothing that user could not already reach.
    // Not an embedded spawn-floor program, so the list lives only in this
    // pin.
    const TERMINAL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::PROC_SPAWN,
        CapabilityId::SHM,
    ];

    // The windowed file viewer `viewer` (plans/APPWIN.md AW5): console
    // write for its fail-loud diagnostics and CAP_SHM to create and
    // grant the zero-copy window frame region the desktop session
    // maps — and deliberately NO filesystem capability: its only reach
    // into the filesystem is the one file the user hands it through
    // the session's trusted picker (the CU6 one-shot fd_grant
    // delegation, redeemed with the unprivileged fd_redeem). Not an
    // embedded spawn-floor program, so the list lives only in this pin.
    const VIEWER_REQUEST: &[CapabilityId] = &[CapabilityId::CONSOLE_WRITE, CapabilityId::SHM];

    // The windowed desktop-backdrop chooser `wallpaper` (plans/PINBOARD.md
    // P9): console write for its fail-loud diagnostics, filesystem reach to
    // list the read-only shipped wallpaper store and read the launching
    // user's own pinboard settings document, `CAP_SHM` to create and grant
    // the zero-copy window frame region the desktop session maps, and
    // `CAP_PROC_SPAWN` to host its own thumbnail-rendering sandbox worker (a
    // restricted spawn of this same binary in its worker role, which the
    // kernel brands capability-empty, so an untrusted wallpaper never
    // decodes in the chooser's own address space). It requests no authority
    // to *write* the settings: adopting a change is the desktop session's
    // decision, asked for over the pinboard rendezvous. Not an embedded
    // spawn-floor program, so the list lives only in this pin.
    const WALLPAPER_CHOOSER_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::SHM,
        CapabilityId::PROC_SPAWN,
    ];

    // The volume-detach tool `unmount` (plans/DEVICES.md D4b): the
    // pure-tool request plus `CAP_FS_MOUNT`, which *is* its job — the
    // kernel's `volume_detach` path re-checks it and audits every
    // decision. Not an embedded spawn-floor program, so the list lives
    // only in this pin.
    const UNMOUNT_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::FS_MOUNT,
    ];

    // The socket-listing tool `ss` (plans/NETWORK.md N8b-2): console write
    // for its output/diagnostics, filesystem access to read its own bundle
    // Help/ payload, and `CAP_SYSINFO_GLOBAL` for the system-wide
    // `NET_SOCKETS` query the listing renders (the socket table names every
    // principal's sockets, so it is privileged and audited). Not an embedded
    // spawn-floor program, so the list lives only in this pin.
    const SS_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::SYSINFO_GLOBAL,
    ];

    // The `ping` tool (plans/NETWORK.md N8b-2b): console write for its
    // output/diagnostics, filesystem access to read its own bundle Help/
    // payload, and `CAP_NET` + `CAP_NET_RAW` to open the ICMP/ICMPv6 echo
    // socket it pings with (the stack re-checks both and audits the open).
    // Not an embedded spawn-floor program, so the list lives only in this
    // pin.
    const PING_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::NET,
        CapabilityId::NET_RAW,
    ];

    // The `host` DNS-lookup tool (plans/DNS.md DNS3): console write for its
    // answers and diagnostics, filesystem access for its own Help/ documents,
    // and CAP_NET for the ordinary UDP socket the stub resolver queries the
    // recursive DNS servers with (the stack re-checks it and audits the open).
    // The active-server set is read through the ungated NET_RESOLVER_SERVERS
    // query, so no CAP_SYSINFO_* is requested. Not an embedded spawn-floor
    // program, so the list lives only in this pin.
    const HOST_TOOL_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::FS_ACCESS,
        CapabilityId::NET,
    ];

    // The `widgets` gallery (plans/GUI-CONTROLS-DESIGN.md): console write for
    // its fail-loud stderr diagnostics, and `CAP_SHM` for the zero-copy window
    // frame region it creates and grants to the desktop session. It reads no
    // filesystem and spawns nothing (every widget commits its action back into
    // the demo itself, never a privileged service). Not an embedded spawn-floor
    // program, so the list lives only in this pin.
    const WIDGETS_GALLERY_REQUEST: &[CapabilityId] =
        &[CapabilityId::CONSOLE_WRITE, CapabilityId::SHM];

    // The Switchboard monitor service (plans/NEW-TASKBAR.md T10/T11): console
    // write for its fail-loud stderr diagnostics, and the two sysinfo reads
    // its sampler has code paths for — `CAP_SYSINFO_GLOBAL` (the system-wide
    // process list) and `CAP_SYSINFO_KERNEL` (the memory-pressure bands),
    // both optional features that degrade to the self-scoped view when the
    // launching user's ceiling strips them. `CAP_SHM` creates and grants the
    // zero-copy frame region the desktop session maps for its overview
    // window, exactly as any other windowed program; `CAP_PROC_CONTROL`
    // carries the overview's force-quit of an owner it did not spawn, and
    // without it that control simply renders refused. `CAP_SYSTEM_POWER`
    // makes this small service — not the large, exposed desktop session —
    // the one holder of the authority to end the machine's power state, on
    // behalf of the session's confirmed quick-actions choice; an ordinary
    // account's ceiling strips it and the rows render refused. It publishes
    // over the ungated `ipc_call` and holds no filesystem or spawn authority
    // — a window is raised or re-launched by asking the session, never the
    // kernel. Not an embedded spawn-floor program, so the list lives only in
    // this pin.
    const SWITCHBOARD_MONITOR_REQUEST: &[CapabilityId] = &[
        CapabilityId::CONSOLE_WRITE,
        CapabilityId::SYSINFO_GLOBAL,
        CapabilityId::SYSINFO_KERNEL,
        CapabilityId::SHM,
        CapabilityId::PROC_CONTROL,
        CapabilityId::SYSTEM_POWER,
    ];

    /// Every program crate's on-disk `AppInfo.toml` manifest source
    /// requests exactly the capability set this registry embeds, and the
    /// two program inventories are identical (`plans/APPS.md` deliverable
    /// 8). The on-disk sources are the migration's single authorship
    /// point; this pin guarantees the still-embedded registry cannot
    /// silently diverge from them before it is deleted.
    #[test]
    fn appinfo_sources_match_the_embedded_registry() {
        use tairix_itest_harness::app_image::discover_app_manifests;

        let userland = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../userland");
        let discovered = discover_app_manifests(&userland).expect("discovery walks");

        // name -> (kind, embedded capability list). PID 1 `init` is
        // deliberately absent: it is the boot floor the boot path enters
        // directly, never a store bundle.
        let embedded: &[(&str, ProgramKind, &[CapabilityId])] = &[
            ("applib", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("basename", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("cat", ProgramKind::Command, CAT_MANIFEST),
            ("chmod", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("clear", ProgramKind::Command, CLEAR_MANIFEST),
            ("configure", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("cp", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("desktop", ProgramKind::Application, DESKTOP_SESSION_REQUEST),
            ("devmgr", ProgramKind::Service, DEVMGR_MANIFEST),
            ("df", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("dirname", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("du", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("edit", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("elsh", ProgramKind::Command, SHELL_MANIFEST),
            ("false", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("files", ProgramKind::Application, FILES_BROWSER_REQUEST),
            ("fontd", ProgramKind::Service, FONTD_MANIFEST),
            ("fstree", ProgramKind::Command, SANDBOXED_FILE_TOOL_REQUEST),
            ("groupadd", ProgramKind::Command, ADMIN_TOOL_REQUEST),
            ("head", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("host", ProgramKind::Command, HOST_TOOL_REQUEST),
            ("login", ProgramKind::Service, LOGIN_MANIFEST),
            ("ls", ProgramKind::Command, LS_MANIFEST),
            ("lspci", ProgramKind::Command, HW_LIST_TOOL_REQUEST),
            ("lsusb", ProgramKind::Command, HW_LIST_TOOL_REQUEST),
            ("man", ProgramKind::Command, MAN_MANIFEST),
            ("mdadm", ProgramKind::Command, RAID_ADMIN_TOOL_REQUEST),
            ("mkdir", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("mv", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("netstack", ProgramKind::Service, NETSTACK_MANIFEST),
            ("ping", ProgramKind::Command, PING_TOOL_REQUEST),
            ("printf", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("ps", ProgramKind::Command, PS_MANIFEST),
            ("reset", ProgramKind::Command, RESET_MANIFEST),
            ("rm", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("rmdir", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("seatmgr", ProgramKind::Service, SEATMGR_MANIFEST),
            ("seq", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("sleep", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("ss", ProgramKind::Command, SS_TOOL_REQUEST),
            ("stress", ProgramKind::Command, STRESS_MANIFEST),
            (
                "switchboard",
                ProgramKind::Service,
                SWITCHBOARD_MONITOR_REQUEST,
            ),
            ("sysinfo", ProgramKind::Command, SYSINFO_MANIFEST),
            ("sysinfod", ProgramKind::Service, SYSINFOD_MANIFEST),
            ("sysmon", ProgramKind::Command, SYSMON_MANIFEST),
            ("tail", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("tee", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("terminal", ProgramKind::Application, TERMINAL_REQUEST),
            ("top", ProgramKind::Command, TOP_MANIFEST),
            ("true", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("unmount", ProgramKind::Command, UNMOUNT_TOOL_REQUEST),
            ("useradd", ProgramKind::Command, ADMIN_TOOL_REQUEST),
            ("users", ProgramKind::Command, USERS_TOOL_MANIFEST),
            ("viewer", ProgramKind::Application, VIEWER_REQUEST),
            ("vim", ProgramKind::Command, FILE_TOOL_REQUEST),
            (
                "wallpaper",
                ProgramKind::Application,
                WALLPAPER_CHOOSER_REQUEST,
            ),
            ("wc", ProgramKind::Command, FILE_TOOL_REQUEST),
            ("whoami", ProgramKind::Command, PURE_TOOL_REQUEST),
            ("widgets", ProgramKind::Application, WIDGETS_GALLERY_REQUEST),
            ("yes", ProgramKind::Command, PURE_TOOL_REQUEST),
        ];

        assert_registry_matches(&discovered, embedded);
    }

    /// Assert the discovered on-disk `AppInfo.toml` inventory and the
    /// embedded registry agree entry for entry — name, kind, and exact
    /// capability set. Split from the pin test so the (long) inventory
    /// table stays readable in one place.
    fn assert_registry_matches(
        discovered: &[tairix_itest_harness::app_image::DiscoveredApp],
        embedded: &[(&str, ProgramKind, &[CapabilityId])],
    ) {
        assert_eq!(discovered.len(), embedded.len());
        for ((name, kind, caps), found) in embedded.iter().zip(discovered) {
            assert_eq!(found.manifest.name, *name, "inventory drift");
            assert_eq!(found.manifest.kind, *kind, "{name}: store drift");
            assert_eq!(
                set(&found.manifest.capabilities),
                set(caps),
                "{name}: capability drift between AppInfo.toml and the registry"
            );
        }
    }
}
