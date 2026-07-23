//! Pure command/verification logic of the `tcpserve` TCP-listener fixture
//! (`plans/NETWORK.md` N6b-2-β-2).
//!
//! The consuming `Run` binary (`src/run.rs`) binds a **privileged** TCP port,
//! listens, accepts the host client peer's connection, echoes every received
//! byte back in order, and verifies the received run matches the shared
//! deterministic stream. This library owns the parts of that fixture a host
//! test can pin with no kernel: the command word, the exact report marker the
//! consuming vertical's serial script keys on, and the shared transfer length
//! and byte verification (the latter re-exported from
//! [`tairix_test_netstack_wire`], so the host client, this server, and the
//! echo-integrity check cannot disagree about a single byte). Keeping them
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
pub const COMMAND: &str = "tcpserve";

/// Leading marker of the success report line. The consuming vertical's serial
/// script waits for this exact prefix before typing the shell `exit` that
/// completes the PASS chain, so it lives here beside the program that emits
/// it. A drifted marker makes the vertical time out loudly rather than pass
/// on the wrong exchange.
pub const PASS_MARKER: &str = "TCPSERVE PASS";

/// Total bytes the host client streams to this server (which echoes them all
/// back) — the shared transfer length both ends derive from one constant.
pub const TRANSFER_BYTES: usize = wire::STREAM_TRANSFER_BYTES;

// The deterministic chunk verification is the one shared definition in the
// wire crate (both TCP fixtures and both host peers use it, so a sender and a
// verifier can never disagree about a byte). Re-exported so this fixture's
// program and tests name it here unchanged. The server only *verifies* the
// bytes it receives (the host client generates and re-verifies the echo), so
// only `verify_chunk` is re-exported, not `fill_chunk`.
pub use wire::verify_chunk;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_matches_the_manifest_name() {
        // The manifest source names the bundle `tcpserve`; the spawn path
        // derives the audited `comm` from that stem, and the vertical keys
        // its PASS on it, so the constant here must match the manifest.
        let manifest = include_str!("../AppInfo.toml");
        assert_eq!(COMMAND, "tcpserve");
        assert!(
            manifest.contains("name = \"tcpserve\""),
            "AppInfo.toml must name the bundle `{COMMAND}`"
        );
    }

    #[test]
    fn manifest_requests_the_privileged_bind_capability() {
        // Binding the well-known GUEST_TCP_PORT needs CAP_NET_BIND_PRIVILEGED
        // on top of CAP_NET; the manifest must request it (intersected with
        // the launching administrator's ceiling, which now grants it).
        let manifest = include_str!("../AppInfo.toml");
        assert!(manifest.contains("CAP_NET_BIND_PRIVILEGED"));
        assert!(manifest.contains("CAP_NET"));
    }

    #[test]
    fn command_is_the_expected_word() {
        assert_eq!(COMMAND, "tcpserve");
        assert!(PASS_MARKER.starts_with("TCPSERVE"));
    }
}
