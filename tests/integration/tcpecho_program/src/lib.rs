//! Pure stream-generation and verification logic of the `tcpecho`
//! stream-socket fixture (`plans/NETWORK.md` N5c).
//!
//! The consuming `Run` binary (`src/run.rs`) opens a TCP stream socket,
//! connects to the host peer's echo server, streams a fixed deterministic
//! byte run, and verifies the peer echoes every byte back in order. This
//! library owns the parts of that fixture a host test can pin with no kernel:
//! the command word, the transfer's deterministic byte generation and
//! offset-checked verification (both over the one shared
//! [`tairix_test_netstack_wire::stream_byte`] generator, so the client and
//! the host echo server cannot disagree about a single byte), and the exact
//! report marker the consuming vertical's serial script keys on. Keeping them
//! here means the program, the vertical's script marker, and the unit tests
//! all read one definition and cannot drift.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_test_netstack_wire as wire;

/// The fixture's command word — the `AppInfo.toml` bundle name, the word the
/// consuming vertical's script types at the shell, and the `comm` the kernel
/// attests on the fixture's audited `exit` (the spawn path names a bundle
/// process by its command stem). One definition, pinned against the manifest
/// source by a host test, so the program, the manifest, and the vertical's
/// PASS keying cannot drift.
pub const COMMAND: &str = "tcpecho";

/// Leading marker of the success report line. The consuming vertical's serial
/// script waits for this exact prefix before typing the shell `exit` that
/// completes the PASS chain, so it lives here beside the program that emits
/// it. A drifted marker makes the vertical time out loudly rather than pass
/// on the wrong exchange.
pub const PASS_MARKER: &str = "TCPECHO PASS";

/// Total bytes the client streams and expects echoed back — the shared
/// transfer length both ends derive from one constant.
pub const TRANSFER_BYTES: usize = wire::STREAM_TRANSFER_BYTES;

// The deterministic chunk fill/verify are the one shared definition in the
// wire crate (both TCP fixtures and both host peers use them, so a sender and
// a verifier can never disagree about a byte). Re-exported so this fixture's
// program and tests name them here unchanged.
pub use wire::{fill_chunk, verify_chunk};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_matches_the_manifest_name() {
        // The manifest source names the bundle `tcpecho`; the spawn path
        // derives the audited `comm` from that stem, and the vertical keys
        // its PASS on it, so the constant here must match the manifest.
        let manifest = include_str!("../AppInfo.toml");
        assert_eq!(COMMAND, "tcpecho");
        assert!(
            manifest.contains("name = \"tcpecho\""),
            "AppInfo.toml must name the bundle `{COMMAND}`"
        );
    }

    #[test]
    fn command_is_the_expected_word() {
        assert_eq!(COMMAND, "tcpecho");
        assert!(PASS_MARKER.starts_with("TCPECHO"));
    }
}
