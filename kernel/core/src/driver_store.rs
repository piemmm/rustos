//! Boot-time enumeration of the `/System/Drivers/` signed-driver store off
//! the mounted root volume (`AGENTS.md` §18.3 / §18.6, `plans/PI.md` P10
//! Stage 4.HW item 5).
//!
//! RustOS does not ship a compiled-in list of *which* drivers exist
//! (`AGENTS.md` §18.6): the discovered driver set is found at runtime by
//! scanning the installed signed bundles under `/System/Drivers/` and
//! reading each bundle's manifest bind table. [`enumerate_driver_store`]
//! is the kernel's half of that scan — the *path enumeration* that turns
//! the on-disk store tree into the list of bundle image paths the
//! user-space driver-store scan (`rustos_drvhost::store::scan_store`)
//! reads, bind-decodes, and hands to the `devmgr` autoloader.
//!
//! It mirrors [`crate::users::load_users_db`]: given the live
//! [`FilesystemRead`] + [`FilesystemSecurity`] driver of the mounted root
//! volume (rustfs on a real installation), it builds a minimal root-backed
//! VFS (`crate::fs::root_backed_vfs`) and walks [`DRIVER_STORE_PATH`]
//! through the VFS's §5.3-checked per-inode delegation, collecting the
//! path of every regular file it finds.
//!
//! # What the walk does — and what it deliberately does not
//!
//! The walk is *structural path discovery only*. It yields the image path
//! of each regular file under `/System/Drivers/` (the store tree is
//! organised `<class>[/<vendor>]/<driver>`, §16.2 / §8); it does **not**
//! read, parse, signature-verify, or otherwise trust a bundle. That is the
//! load gate's job (`rustos_drvhost::Host::load`), run only when — and
//! only when — a candidate wins a hardware-tree node (`AGENTS.md` §18.6).
//!
//! # Fail closed, never fatal (`AGENTS.md` §18.4 / §5.4 / §2.9)
//!
//! Every refusal is contained to the offending entry: a sub-directory that
//! cannot be listed, an entry that cannot be `stat`-ed, a name that is not
//! a single well-formed path component, or anything past the bounds below
//! is **skipped** (counted, not collected) and the walk continues. A
//! `/System/Drivers/` that does not exist is not an error — a headless or
//! driverless install simply enumerates nothing and autoloads nothing
//! (`AGENTS.md` §18.4). The scan yields whatever well-formed paths it
//! found; it never panics and never aborts the boot.
//!
//! # Credentials of the boot read
//!
//! Like the users-database read, the walk runs under the kernel's
//! bootstrap identity — `uid 0`, `gid 0`, **no** capabilities — which
//! carries no ambient power (`AGENTS.md` §5.1). The store directories are
//! reachable because their stored §5.3 records make them searchable to
//! that identity, not because the kernel bypasses the check.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity, NodeKind};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{Field, Level, Sink};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{Credentials, Path, Vfs};

/// Absolute path of the signed-driver store on the root volume
/// (`AGENTS.md` §16.2 — drivers live under `/System/Drivers/`).
pub const DRIVER_STORE_PATH: &str = "/System/Drivers";

/// Maximum directory depth the walk descends *below* [`DRIVER_STORE_PATH`].
///
/// The store tree is `<class>[/<vendor>]/<driver>` (§16.2 / §8), at most a
/// few levels deep; this is a fail-closed validation bound (`AGENTS.md`
/// §24.4 — a defence against a malformed or hostile on-disk tree, not a
/// scalable capacity), not a limit a legitimate store ever reaches. A
/// node deeper than this is skipped.
pub const MAX_STORE_DEPTH: usize = 8;

/// Maximum number of driver bundle image paths the walk collects.
///
/// A fail-closed validation bound (`AGENTS.md` §24.4): a store presenting
/// more entries than this is malformed, and the surplus is skipped rather
/// than allowed to grow the scan without limit.
pub const MAX_STORE_DRIVERS: usize = 256;

/// Enumerate the `/System/Drivers/` signed-driver store, returning the
/// image path of every driver bundle found (`AGENTS.md` §18.3 / §18.6).
///
/// The returned paths are absolute and rooted at [`DRIVER_STORE_PATH`],
/// in the driver's on-disk enumeration order. Each is understood verbatim
/// by the user-space scan (`rustos_drvhost::store::scan_store`, which
/// reads and bind-decodes the bundle) and by the load gate
/// (`rustos_drvhost::Host::load`, which verifies it). This function does
/// none of that — it only finds the paths.
///
/// A single [`AuditEvent::DriverStoreScanned`] record is emitted with the
/// count of paths found and the count of entries skipped fail-closed
/// (`AGENTS.md` §5.4.4). The scan never errors: a missing store, an
/// unreadable sub-directory, or a malformed entry all simply contribute
/// fewer paths (`AGENTS.md` §18.4 / §2.9).
#[must_use]
pub fn enumerate_driver_store<F>(fs: &mut F, audit: &dyn Sink) -> Vec<String>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut drivers: Vec<String> = Vec::new();
    let mut skipped: usize = 0;

    match crate::fs::root_backed_vfs() {
        Ok(vfs) => {
            let caps = CapabilitySet::empty();
            let cred = Credentials {
                uid: UserId(0),
                gid: GroupId(0),
                supplementary_gids: &[],
                caps: &caps,
            };
            walk_dir(
                &vfs,
                &cred,
                fs,
                DRIVER_STORE_PATH,
                0,
                &mut drivers,
                &mut skipped,
            );
        }
        // The private root mount could not be built; nothing to scan. The
        // walk is fail-closed (`AGENTS.md` §2.9), so this surfaces as an
        // empty store rather than a panic.
        Err(_) => skipped += 1,
    }

    audit_scan(audit, drivers.len(), skipped);
    drivers
}

/// Recursively collect the regular-file image paths under the directory at
/// `dir`, descending into sub-directories up to [`MAX_STORE_DEPTH`].
///
/// `depth` counts levels below [`DRIVER_STORE_PATH`] (the store root is
/// depth `0`). Every fail-closed refusal increments `skipped` and the walk
/// continues; nothing here returns an error (`AGENTS.md` §18.4 / §5.4).
fn walk_dir<F>(
    vfs: &Vfs,
    cred: &Credentials<'_>,
    fs: &mut F,
    dir: &str,
    depth: usize,
    drivers: &mut Vec<String>,
    skipped: &mut usize,
) where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let Ok(dir_path) = Path::parse(dir) else {
        *skipped += 1;
        return;
    };

    // A directory the boot identity may not list, a driver fault, or a
    // store that simply does not exist all leave this subtree empty
    // (`AGENTS.md` §18.4). A non-root listing failure is a skipped entry;
    // a missing store root is the legitimate "no drivers" case and is not
    // counted, because the empty result already says so.
    let names = match vfs.list_via_secured(cred, &dir_path, fs) {
        Ok(names) => names,
        Err(_) if depth == 0 => return,
        Err(_) => {
            *skipped += 1;
            return;
        }
    };

    for name in names {
        if drivers.len() >= MAX_STORE_DRIVERS {
            // The store presents more entries than the validation bound
            // permits; the surplus is refused fail-closed (`AGENTS.md`
            // §24.4) rather than growing the scan without limit.
            *skipped += 1;
            continue;
        }

        let child = if dir == "/" {
            let mut s = String::with_capacity(1 + name.len());
            s.push('/');
            s.push_str(&name);
            s
        } else {
            let mut s = String::with_capacity(dir.len() + 1 + name.len());
            s.push_str(dir);
            s.push('/');
            s.push_str(&name);
            s
        };

        let Ok(child_path) = Path::parse(&child) else {
            *skipped += 1;
            continue;
        };
        // Defend against a driver returning a name that is not a single
        // path component (an embedded `/`, an empty or dotted token): the
        // child must add exactly one level to its parent, or it is refused
        // (`AGENTS.md` §5.4 — validate every input).
        if child_path.depth() != dir_path.depth() + 1 {
            *skipped += 1;
            continue;
        }

        match vfs.stat_via_secured(cred, &child_path, fs) {
            Ok(info) => match info.kind {
                NodeKind::RegularFile => drivers.push(child),
                NodeKind::Directory => {
                    if depth >= MAX_STORE_DEPTH {
                        // Deeper than the validation bound; refuse rather
                        // than recurse without limit (`AGENTS.md` §24.4).
                        *skipped += 1;
                    } else {
                        walk_dir(vfs, cred, fs, &child, depth + 1, drivers, skipped);
                    }
                }
            },
            Err(_) => *skipped += 1,
        }
    }
}

fn audit_scan(audit: &dyn Sink, drivers: usize, skipped: usize) {
    let mut drivers_buf = [0u8; 12];
    let mut skipped_buf = [0u8; 12];
    emit(
        audit,
        Level::Info,
        AuditEvent::DriverStoreScanned,
        &[
            Field {
                key: "path",
                value: DRIVER_STORE_PATH,
            },
            Field {
                key: "drivers",
                value: format_usize(drivers, &mut drivers_buf),
            },
            Field {
                key: "skipped",
                value: format_usize(skipped, &mut skipped_buf),
            },
        ],
    );
}

#[cfg(test)]
#[path = "driver_store_tests.rs"]
mod tests;
