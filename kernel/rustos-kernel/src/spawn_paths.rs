//! The absolute paths the embedded programs are registered under — the one
//! OS-wide path contract the `spawn` syscall resolves byte-for-byte, with
//! no prefix or alias resolution.
//!
//! Every program — command app and service alike — is a `<name>.app`
//! bundle (a service is an app): a command app lives in the system app
//! store as `/System/Apps/<command>.app/Run`, so the shell's bare-word
//! resolution (`plans/APPS.md` §8, built from the shared `rustos_abi`
//! store definitions) lands on a registered path, and a service lives in
//! the service store as `/System/Services/<name>.app/Run`, the path PID 1
//! `init`'s startup config names. Pure data, free of the rxe-laden
//! registry rows in `spawn_layout` that consume it, so it compiles — and
//! its store-spelling drift test runs — on the CI host as well as on each
//! bare-metal production build.

/// Absolute path the `elsh` shell program is registered under: the system
/// app store's command-named bundle (`plans/APPS.md` §8). It must match
/// exactly the shell path an authenticated account's
/// `/System/Security/Users` record names. One OS-wide path contract,
/// identical on every target, so it lives here once.
pub const SHELL_PATH: &[u8] = b"/System/Apps/elsh.app/Run";

/// Absolute path the login service program is registered under
/// (`plans/PI.md` P11): the service store's `<name>.app` bundle (a service
/// is an app). It must match exactly the `session` path PID 1 `init` reads
/// from its startup config and hands to the `spawn` syscall
/// (`userland/system/init/src/startup.rs`). One OS-wide path contract,
/// identical on every target.
pub const LOGIN_PATH: &[u8] = b"/System/Services/login.app/Run";

/// Absolute path the device-manager service program is registered under:
/// the service store's `<name>.app` bundle. It must match exactly the
/// device-manager path PID 1 `init` hands to the `spawn` syscall at
/// startup (`userland/system/init/src/startup.rs`). One OS-wide path
/// contract, identical on every target.
pub const DEVMGR_PATH: &[u8] = b"/System/Services/devmgr.app/Run";

/// Absolute path the System Information service program is registered under
/// (`AGENTS.md` §16.6): the service store's `<name>.app` bundle. It must
/// match exactly the `sysinfod` path PID 1 `init` hands to the `spawn`
/// syscall at startup (`userland/system/init/src/startup.rs`). One OS-wide
/// path contract, identical on every target.
pub const SYSINFOD_PATH: &[u8] = b"/System/Services/sysinfod.app/Run";

/// Absolute path the seat-manager service program is registered under
/// (`plans/DISPLAY.md` D3): the service store's `<name>.app` bundle. It
/// must match exactly the `seatmgr` path PID 1 `init` hands to the `spawn`
/// syscall at startup (`userland/system/init/src/startup.rs`). One OS-wide
/// path contract, identical on every target.
pub const SEATMGR_PATH: &[u8] = b"/System/Services/seatmgr.app/Run";

/// Absolute path the network-stack service program is registered under
/// (`plans/NETWORK.md` §2.2): the service store's `<name>.app` bundle. It
/// must match exactly the `netstack` path PID 1 `init` hands to the `spawn`
/// syscall at startup (`userland/system/init/src/startup.rs`). One OS-wide
/// path contract, identical on every target.
pub const NETSTACK_PATH: &[u8] = b"/System/Services/netstack.app/Run";

/// Absolute path the `ps` tool program is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `ps`
/// to it (`plans/APPS.md` §8). One OS-wide path contract, identical on
/// every target.
pub const PS_PATH: &[u8] = b"/System/Apps/ps.app/Run";

/// Absolute path the `sysinfo` tool program is registered under: the system
/// app store's command-named bundle, so the shell resolves the bare word
/// `sysinfo` to it (`plans/APPS.md` §8). One OS-wide path contract,
/// identical on every target.
pub const SYSINFO_PATH: &[u8] = b"/System/Apps/sysinfo.app/Run";

/// Absolute path the `sysmon` tool program is registered under: the system
/// app store's command-named bundle, so the shell resolves the bare word
/// `sysmon` to it (`plans/APPS.md` §8). One OS-wide path contract,
/// identical on every target.
pub const SYSMON_PATH: &[u8] = b"/System/Apps/sysmon.app/Run";

/// Absolute path the `stress` load-generator tool is registered under: the
/// system app store's command-named bundle, so the shell resolves the bare
/// word `stress` to it (`plans/APPS.md` §8). One OS-wide path contract,
/// identical on every target.
pub const STRESS_PATH: &[u8] = b"/System/Apps/stress.app/Run";

/// Absolute path the `top` tool program is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `top`
/// to it (`plans/APPS.md` §8). One OS-wide path contract, identical on
/// every target.
pub const TOP_PATH: &[u8] = b"/System/Apps/top.app/Run";

/// Absolute path the `ls` tool program is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `ls`
/// to it (`plans/APPS.md` §8). One OS-wide path contract, identical on
/// every target.
pub const LS_PATH: &[u8] = b"/System/Apps/ls.app/Run";

/// Absolute path the `cat` tool program is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `cat`
/// to it (`plans/APPS.md` §8). One OS-wide path contract, identical on
/// every target.
pub const CAT_PATH: &[u8] = b"/System/Apps/cat.app/Run";

/// Absolute path the `man` help tool is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `man`
/// to it (`plans/APPS.md` §7–§8). One OS-wide path contract, identical on
/// every target.
pub const MAN_PATH: &[u8] = b"/System/Apps/man.app/Run";

/// Absolute path the `clear` tool program is registered under: the system
/// app store's command-named bundle, so the shell resolves the bare word
/// `clear` to it (`plans/APPS.md` §8). One OS-wide path contract, identical
/// on every target.
pub const CLEAR_PATH: &[u8] = b"/System/Apps/clear.app/Run";

/// Absolute path the `reset` tool program is registered under: the system
/// app store's command-named bundle, so the shell resolves the bare word
/// `reset` to it (`plans/APPS.md` §8). One OS-wide path contract, identical
/// on every target.
pub const RESET_PATH: &[u8] = b"/System/Apps/reset.app/Run";

/// Absolute path the `users` account-administration tool is registered
/// under: the system app store's command-named bundle, so an
/// administrator's shell resolves the bare word `users` to it
/// (`plans/APPS.md` §8, `plans/CAPABILITY_USE.md` CU4). One OS-wide path
/// contract, identical on every target.
pub const USERS_CLI_PATH: &[u8] = b"/System/Apps/users.app/Run";

#[cfg(test)]
mod tests {
    use super::{
        CAT_PATH, CLEAR_PATH, DEVMGR_PATH, LOGIN_PATH, LS_PATH, MAN_PATH, NETSTACK_PATH, PS_PATH,
        RESET_PATH, SEATMGR_PATH, SHELL_PATH, STRESS_PATH, SYSINFOD_PATH, SYSINFO_PATH,
        SYSMON_PATH, TOP_PATH, USERS_CLI_PATH,
    };
    use rustos_abi::{BundleEntry, BUNDLE_SUFFIX, SYSTEM_APP_STORE, SYSTEM_SERVICE_STORE};

    /// The system services PID 1 spawns are registered under the service
    /// store as `<service>.app` bundles (a service is an app), never in the
    /// app store: they are session/service programs, not commands a user
    /// types. The spelling is built from the shared `rustos_abi` store
    /// definitions so this registry and the on-disk bundle layout cannot
    /// drift.
    #[test]
    fn services_live_under_system_services_as_bundles() {
        for (path, service) in [
            (LOGIN_PATH, "login"),
            (DEVMGR_PATH, "devmgr"),
            (SYSINFOD_PATH, "sysinfod"),
            (SEATMGR_PATH, "seatmgr"),
            (NETSTACK_PATH, "netstack"),
        ] {
            let expected = alloc::format!(
                "{SYSTEM_SERVICE_STORE}/{service}{BUNDLE_SUFFIX}/{}",
                BundleEntry::Run.as_str()
            );
            assert_eq!(core::str::from_utf8(path), Ok(expected.as_str()));
        }
    }

    /// Every command app is registered under the system app store as a
    /// command-named bundle, `<store>/<command>.app/Run`, so the shell's
    /// bare-word resolution (which builds the same spelling from the shared
    /// `rustos_abi` definitions) always lands on a registered path. A drift
    /// between this registry and the shared store definition would silently
    /// break every bare command word.
    #[test]
    fn command_apps_live_in_the_system_app_store() {
        for (path, command) in [
            (SHELL_PATH, "elsh"),
            (CAT_PATH, "cat"),
            (CLEAR_PATH, "clear"),
            (LS_PATH, "ls"),
            (MAN_PATH, "man"),
            (PS_PATH, "ps"),
            (RESET_PATH, "reset"),
            (STRESS_PATH, "stress"),
            (SYSINFO_PATH, "sysinfo"),
            (SYSMON_PATH, "sysmon"),
            (TOP_PATH, "top"),
            (USERS_CLI_PATH, "users"),
        ] {
            let expected = alloc::format!(
                "{SYSTEM_APP_STORE}/{command}{BUNDLE_SUFFIX}/{}",
                BundleEntry::Run.as_str()
            );
            assert_eq!(core::str::from_utf8(path), Ok(expected.as_str()));
        }
    }
}
