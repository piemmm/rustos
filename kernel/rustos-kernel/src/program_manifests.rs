//! The manifest-requested capability list of every embedded program the
//! runtime `spawn` syscall resolves, and of PID 1 `init`
//! (`plans/CAPABILITY_USE.md` CU2).
//!
//! Each list is a program's **request** — the manifest side of the
//! `user ceiling ∩ manifest request` intersection the admit path derives a
//! child's effective set from — sized to exactly the gated syscalls the
//! program actually calls, neither wider nor narrower. Requesting wide is
//! not a grant (the account ceiling bounds every session process), but an
//! unexercised request is still unaudited surface, so each list stays
//! minimal. The registry rows in `spawn_layout` consume these lists; the
//! unit tests below pin each one, so a manifest change is a reviewed diff,
//! never an accident.
//!
//! The lists are pure data, free of the baked `rxe` fixtures the registry
//! rows carry, so they compile — and their pinning tests run — on the CI
//! host under `cargo test` as well as on each freestanding production
//! build whose registry consumes them, and on no other configuration, so
//! they are never dead code.

use rustos_abi::CapabilityId;

/// The session baseline (`plans/CAPABILITY_USE.md` §4.2) — what every
/// interactive account's shell requests, and the `Shell` program's whole
/// manifest. The set is account policy shared with the users-database
/// authors (the image builder's seeded grants, the installer), so its one
/// definition lives beside the account record in `lib/users` and is
/// re-exported here for the registry rows; the per-capability rationale
/// lives on that definition.
///
/// Nothing else: `wait`, `signal`, `rlimit_get`/`rlimit_set`, and
/// `fs_getcwd` — the rest of what the shell calls — are ungated.
// Consumed by the registry rows on the row-bearing targets and by the
// exact-set pinning tests on the CI host; the aarch64 production build
// carries no rows and its shell manifest travels in the on-disk bundle.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub use rustos_users::SESSION_BASELINE;

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
/// `CAP_LOG_EMIT` for its structured audit records, and
/// `CAP_SYSINFO_KERNEL` so the full-screen view's bottom bar can show the
/// memory figures (a refusal degrades that one figure to a placeholder,
/// never the login). No `CAP_FS_ACCESS`:
/// login reads the users database through its own gated syscall and never
/// touches the filesystem.
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
];

/// The device-manager service's manifest: `CAP_SYSINFO_HW` for the
/// privileged hardware-tree view (`hw_tree_read`/`hw_tree_wait`),
/// `CAP_DRV_LOAD` for the restricted driver-store `ipc_call` endpoint (the
/// kernel re-checks it at the load gate), and `CAP_LOG_EMIT` for its
/// structured diagnostics. It writes no standard stream (no console pair)
/// and holds no resource capability: the kernel mints a loaded driver's
/// grants from its matched node, never from this caller.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const DEVMGR_MANIFEST: &[CapabilityId] = &[
    CapabilityId::SYSINFO_HW,
    CapabilityId::DRV_LOAD,
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

/// The `ps` tool's manifest: `CAP_CONSOLE_WRITE` for its listing on fd 1
/// and diagnostics on fd 2, plus `CAP_FS_ACCESS` because its short-help
/// switches read the bundle's own `Help/` tree through the secured VFS
/// (which still authorises every path per-inode under the caller's
/// attested identity). Every per-query scope is enforced by `sysinfod`
/// against this process's kernel-attested origin.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const PS_MANIFEST: &[CapabilityId] = &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

/// The `sysinfo` tool's manifest: like `ps`, `CAP_CONSOLE_WRITE` for the
/// rendered results on fd 1 plus `CAP_FS_ACCESS` because its short-help
/// switches read the bundle's own `Help/` tree through the secured VFS
/// (which still authorises every path per-inode under the caller's
/// attested identity); per-query authority stays with `sysinfod` and the
/// caller's attested origin.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const SYSINFO_MANIFEST: &[CapabilityId] =
    &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

/// The `top` tool's manifest: `CAP_CONSOLE_WRITE` for its full-screen
/// display on fd 1, `CAP_CONSOLE_READ` for raw-mode keystrokes on fd 0
/// (the latter also authorises its `stream_input_mode` raw discipline), and
/// `CAP_FS_ACCESS` because its short-help switches read the bundle's own
/// `Help/` tree through the secured VFS (which still authorises every
/// path per-inode under the caller's attested identity); `terminal_size`
/// is ungated and per-query `sysinfo` scope is enforced by `sysinfod`.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const TOP_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
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
/// raw discipline, as in `top`), and `CAP_FS_ACCESS` because reading a
/// bundle's `Help/` documents *is* the tool's job — the secured VFS still
/// authorises every path per-inode under the caller's attested identity.
#[cfg(any(test, not(all(freestanding, kernel_isa = "aarch64"))))]
pub const MAN_MANIFEST: &[CapabilityId] = &[
    CapabilityId::CONSOLE_WRITE,
    CapabilityId::CONSOLE_READ,
    CapabilityId::FS_ACCESS,
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
/// (`stream_write`) and `CAP_PROC_SPAWN` to launch the boot services and
/// the per-console login supervisors. As a system program its manifest is
/// also its ceiling (there is no users-db row for the system principal),
/// and each child it spawns is bounded by that child's *own* registered
/// manifest, never by this set.
pub const INIT_MANIFEST: &[CapabilityId] = &[CapabilityId::CONSOLE_WRITE, CapabilityId::PROC_SPAWN];

#[cfg(test)]
mod tests {
    //! Pinning tests: one exact-set assertion per manifest, so widening or
    //! narrowing any program's request is a reviewed test diff, never an
    //! accident — plus the invariant that every session *tool* requests
    //! within the session baseline, so a baseline-only account can run the
    //! whole default toolset.

    use rustos_caps::CapabilitySet;

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
    fn shell_manifest_is_exactly_the_session_baseline() {
        assert_eq!(
            set(SESSION_BASELINE),
            set(&[
                CapabilityId::FS_ACCESS,
                CapabilityId::PROC_SPAWN,
                CapabilityId::CONSOLE_WRITE,
                CapabilityId::CONSOLE_READ,
            ])
        );
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
    fn ps_manifest_is_pinned() {
        assert_eq!(
            set(PS_MANIFEST),
            set(&[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS])
        );
    }

    #[test]
    fn sysinfo_manifest_is_pinned() {
        assert_eq!(
            set(SYSINFO_MANIFEST),
            set(&[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS])
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
            set(&[CapabilityId::CONSOLE_WRITE, CapabilityId::PROC_SPAWN])
        );
    }

    /// Every session tool a shell spawns requests within the session
    /// baseline, so an account granted only the baseline can run the whole
    /// default toolset: each tool's `manifest ∩ ceiling` intersection
    /// loses nothing. The `users` tool is deliberately absent: its
    /// `CAP_USER_ADMIN` request sits above the baseline, so it works only
    /// for an administrator account (the administrative set carries it)
    /// and is inert — not missing — for everyone else.
    #[test]
    fn session_tools_request_within_the_session_baseline() {
        let baseline = set(SESSION_BASELINE);
        for manifest in [
            CAT_MANIFEST,
            CLEAR_MANIFEST,
            LS_MANIFEST,
            PS_MANIFEST,
            RESET_MANIFEST,
            SYSINFO_MANIFEST,
            TOP_MANIFEST,
        ] {
            for cap in manifest {
                assert!(baseline.contains(*cap), "{cap:?} exceeds the baseline");
            }
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
        let ceiling = rustos_users::administrator_ceiling();
        for manifest in [USERS_TOOL_MANIFEST, ADMIN_TOOL_REQUEST] {
            for cap in manifest {
                assert!(ceiling.contains(*cap), "{cap:?} exceeds the admin ceiling");
            }
        }
    }

    /// Every program crate's on-disk `AppInfo.toml` manifest source
    /// requests exactly the capability set this registry embeds, and the
    /// two program inventories are identical (`plans/APPS.md` deliverable
    /// 8). The on-disk sources are the migration's single authorship
    /// point; this pin guarantees the still-embedded registry cannot
    /// silently diverge from them before it is deleted.
    #[test]
    fn appinfo_sources_match_the_embedded_registry() {
        use rustos_itest_harness::app_image::{discover_app_manifests, AppKind};

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

        // The store-only pure text tools' expected request: console write
        // for their output and the filesystem gate their short-help read
        // needs — they touch no operand path and never prompt. They ship
        // purely as discovered on-disk bundles — the boot floor never
        // grows — so no `spawn_layout` row or manifest constant exists for
        // them and their `AppInfo.toml` is pinned here directly.
        const PURE_TOOL_REQUEST: &[CapabilityId] =
            &[CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS];

        let userland = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../userland");
        let discovered = discover_app_manifests(&userland).expect("discovery walks");

        // name -> (kind, embedded capability list). PID 1 `init` is
        // deliberately absent: it is the boot floor the boot path enters
        // directly, never a store bundle.
        let embedded: &[(&str, AppKind, &[CapabilityId])] = &[
            ("basename", AppKind::Command, PURE_TOOL_REQUEST),
            ("cat", AppKind::Command, CAT_MANIFEST),
            ("clear", AppKind::Command, CLEAR_MANIFEST),
            ("cp", AppKind::Command, FILE_TOOL_REQUEST),
            ("devmgr", AppKind::Service, DEVMGR_MANIFEST),
            ("dirname", AppKind::Command, PURE_TOOL_REQUEST),
            ("elsh", AppKind::Command, SESSION_BASELINE),
            ("false", AppKind::Command, PURE_TOOL_REQUEST),
            ("groupadd", AppKind::Command, ADMIN_TOOL_REQUEST),
            ("login", AppKind::Service, LOGIN_MANIFEST),
            ("ls", AppKind::Command, LS_MANIFEST),
            ("man", AppKind::Command, MAN_MANIFEST),
            ("mv", AppKind::Command, FILE_TOOL_REQUEST),
            ("ps", AppKind::Command, PS_MANIFEST),
            ("reset", AppKind::Command, RESET_MANIFEST),
            ("rm", AppKind::Command, FILE_TOOL_REQUEST),
            ("sysinfo", AppKind::Command, SYSINFO_MANIFEST),
            ("sysinfod", AppKind::Service, SYSINFOD_MANIFEST),
            ("top", AppKind::Command, TOP_MANIFEST),
            ("true", AppKind::Command, PURE_TOOL_REQUEST),
            ("useradd", AppKind::Command, ADMIN_TOOL_REQUEST),
            ("users", AppKind::Command, USERS_TOOL_MANIFEST),
            ("yes", AppKind::Command, PURE_TOOL_REQUEST),
        ];

        assert_eq!(discovered.len(), embedded.len());
        for ((name, kind, caps), found) in embedded.iter().zip(&discovered) {
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
