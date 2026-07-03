//! The absolute paths the embedded programs are registered under — the one
//! OS-wide path contract the `spawn` syscall resolves byte-for-byte, with
//! no prefix or alias resolution.
//!
//! Services live under `/System/Services/`; every command app lives in the
//! system app store as a command-named bundle,
//! `/System/Apps/<command>.app/Run`, so the shell's bare-word resolution
//! (`plans/APPS.md` §8, built from the shared `rustos_abi` store
//! definitions) lands on a registered path. Pure data, free of the
//! rxe-laden registry rows in `spawn_layout` that consume it, so it
//! compiles — and its store-spelling drift test runs — on the CI host as
//! well as on each bare-metal production build.

/// Absolute path the `elsh` shell program is registered under: the system
/// app store's command-named bundle (`plans/APPS.md` §8). It must match
/// exactly the shell path an authenticated account's
/// `/System/Security/Users` record names. One OS-wide path contract,
/// identical on every target, so it lives here once.
pub const SHELL_PATH: &[u8] = b"/System/Apps/elsh.app/Run";

/// Absolute path the login service program is registered under
/// (`plans/PI.md` P11). It must match exactly the `session` path PID 1
/// `init` reads from its startup config and hands to the `spawn` syscall
/// (`userland/system/init/src/startup.rs`). One OS-wide path contract,
/// identical on every target.
pub const LOGIN_PATH: &[u8] = b"/System/Services/login";

/// Absolute path the device-manager service program is registered under. It
/// must match exactly the device-manager path PID 1 `init` hands to the
/// `spawn` syscall at startup (`userland/system/init/src/startup.rs`). One
/// OS-wide path contract, identical on every target.
pub const DEVMGR_PATH: &[u8] = b"/System/Services/devmgr";

/// Absolute path the System Information service program is registered under
/// (`AGENTS.md` §16.6). It must match exactly the `sysinfod` path PID 1
/// `init` hands to the `spawn` syscall at startup
/// (`userland/system/init/src/startup.rs`). One OS-wide path contract,
/// identical on every target.
pub const SYSINFOD_PATH: &[u8] = b"/System/Services/sysinfod";

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

/// Absolute path the `top` tool program is registered under: the system app
/// store's command-named bundle, so the shell resolves the bare word `top`
/// to it (`plans/APPS.md` §8). One OS-wide path contract, identical on
/// every target.
pub const TOP_PATH: &[u8] = b"/System/Apps/top.app/Run";

/// Absolute path the `users` account-administration tool is registered
/// under: the system app store's command-named bundle, so an
/// administrator's shell resolves the bare word `users` to it
/// (`plans/APPS.md` §8, `plans/CAPABILITY_USE.md` CU4). One OS-wide path
/// contract, identical on every target.
pub const USERS_CLI_PATH: &[u8] = b"/System/Apps/users.app/Run";

#[cfg(test)]
mod tests {
    use super::{
        DEVMGR_PATH, LOGIN_PATH, PS_PATH, SHELL_PATH, SYSINFOD_PATH, SYSINFO_PATH, TOP_PATH,
        USERS_CLI_PATH,
    };
    use rustos_abi::{BundleEntry, BUNDLE_SUFFIX, SYSTEM_APP_STORE};

    /// The system services PID 1 spawns are registered under
    /// `/System/Services/`, never in the app store: they are session/service
    /// programs, not commands a user types.
    #[test]
    fn services_live_under_system_services() {
        for (path, service) in [
            (LOGIN_PATH, "login"),
            (DEVMGR_PATH, "devmgr"),
            (SYSINFOD_PATH, "sysinfod"),
        ] {
            let expected = alloc::format!("/System/Services/{service}");
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
            (PS_PATH, "ps"),
            (SYSINFO_PATH, "sysinfo"),
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
