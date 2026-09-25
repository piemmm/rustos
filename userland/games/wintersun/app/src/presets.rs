//! The figure a player walks as: one of the preset records the bundle ships
//! in its own `Resources/`, every one of which the art harness measures
//! before it ships.

use alloc::string::String;

use tairix_abi::{BUNDLE_SUFFIX, SYSTEM_APPLICATION_STORE};
use tairix_wintersun_figure::identity::RECORD_EXTENSION;

/// The name this client's bundle is filed under in the application store.
pub const BUNDLE: &str = "wintersun";

/// The preset a player who has not designed a character walks as.
pub const DEFAULT: &str = "human-wayfarer";

/// Where preset `name`'s record is installed: this bundle's own
/// `Resources/`, spelled from the shared store definitions so it cannot
/// drift from where the image builder plants it.
#[must_use]
pub fn installed(name: &str) -> String {
    alloc::format!(
        "{SYSTEM_APPLICATION_STORE}/{BUNDLE}{BUNDLE_SUFFIX}/Resources/{name}.{RECORD_EXTENSION}"
    )
}

#[cfg(test)]
#[path = "presets_tests.rs"]
mod tests;
