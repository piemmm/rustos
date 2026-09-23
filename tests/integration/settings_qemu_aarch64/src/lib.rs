//! The Settings vertical's shared contract
//! (`plans/NEW-DESKTOP-SETTINGS.md` DS13).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so what the host injects and what the guest latches cannot
//! drift apart.
//!
//! # Who states what
//!
//! - The **host** reads the serial transcript, so it gates each gesture and
//!   each screendump on the desktop session's own announcements: the system
//!   menu drawn, the Settings window's first frame on screen, each pane's
//!   frame on screen (the window wears the pane's title, and the session
//!   announces the frame that carries a new title), and the desktop redrawn in
//!   the appearance chosen.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the bundle a load names, the window channel's create
//!   reply, and each commit of the desktop's published settings document,
//!   attributed by the path it replaced rather than by how many writes have
//!   gone by (`plans/OPEN-DEFECTS.md` D19/D20).
//!
//! Neither side infers the other's facts.
//!
//! Two spellings are borrowed — the desktop's publisher identity and the
//! app-data service's published file name — and generated from their own
//! definitions at build time: linked into the guest, the crates that define
//! them would switch on an allocator in every kernel built beside it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the Settings application — the bundle is
/// `<system application store>/<name>.app`, composed from the shared
/// `lib/abi` spellings on both sides rather than written out here.
pub const SETTINGS_APP_NAME: &str = "settings";

include!(concat!(env!("OUT_DIR"), "/contract.rs"));

/// The `op` field value the kernel records for a rename, which is how the
/// service commits a document: written whole under a sibling name, then
/// renamed over the live one.
pub const RENAME_OP: &str = "rename";

/// How many times the script changes the desktop's appearance: to light
/// through the Settings Appearance pane, then back to dark through the system
/// menu's own row. Each is one commit of the published document, and reaching
/// this many is the guest's PASS.
pub const APPEARANCE_CHANGES: u32 = 2;

/// Whether `path` is the desktop session's own published settings document:
/// `…/<the desktop's publisher>/<the published file>`.
///
/// Matched by the whole trailing pair, so neither the store's ownership pin
/// beside it nor the temporary name a commit is written under can pass for
/// it.
#[must_use]
pub fn is_desktop_document(path: &str) -> bool {
    path.strip_suffix(PUBLISHED_FILE)
        .and_then(|dir| dir.strip_suffix('/'))
        .and_then(|dir| dir.strip_suffix(DESKTOP_PUBLISHER))
        .is_some_and(|parent| parent.ends_with('/'))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::format;

    use super::{is_desktop_document, DESKTOP_PUBLISHER, PUBLISHED_FILE};

    /// Only the desktop's own published document matches: not the temporary
    /// name a commit is written under, not the pin beside it, and not another
    /// application's.
    #[test]
    fn only_the_desktops_published_document_matches() {
        let dir = format!("/Users/ada/Settings/Apps/{DESKTOP_PUBLISHER}");
        assert!(is_desktop_document(&format!("{dir}/{PUBLISHED_FILE}")));
        assert!(!is_desktop_document(&format!("{dir}/{PUBLISHED_FILE}.new")));
        assert!(!is_desktop_document(&format!("{dir}/.owner")));
        assert!(!is_desktop_document(
            "/Users/ada/Settings/Apps/os.tairix.terminal/public.conf"
        ));
        assert!(!is_desktop_document(&format!(
            "/Users/ada/Settings/Apps/x{DESKTOP_PUBLISHER}/{PUBLISHED_FILE}"
        )));
        assert!(!is_desktop_document(&format!(
            "{DESKTOP_PUBLISHER}/{PUBLISHED_FILE}"
        )));
    }
}
