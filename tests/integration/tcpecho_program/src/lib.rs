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

/// Fill `buf` with the deterministic stream bytes starting at stream offset
/// `offset`: `buf[i]` is the byte at absolute stream position `offset + i`.
///
/// The client calls this to produce each outbound chunk; the host peer echoes
/// whatever it receives, so the echoed run is byte-identical to this sequence
/// and [`verify_chunk`] re-derives it to check the echo without buffering the
/// whole transfer.
pub fn fill_chunk(offset: usize, buf: &mut [u8]) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = wire::stream_byte(offset + i);
    }
}

/// Verify a received echo chunk against the deterministic stream: `chunk[i]`
/// must equal the byte at absolute stream position `offset + i`.
///
/// Returns `Ok(())` when every byte matches, or `Err(index)` naming the first
/// mismatched offset *within the chunk* — a corrupted, reordered, or
/// duplicated echo is caught at its first wrong byte rather than accepted
/// (fail closed).
///
/// # Errors
///
/// The chunk-relative index of the first byte that does not match the
/// expected deterministic stream value.
pub fn verify_chunk(offset: usize, chunk: &[u8]) -> Result<(), usize> {
    for (i, &byte) in chunk.iter().enumerate() {
        if byte != wire::stream_byte(offset + i) {
            return Err(i);
        }
    }
    Ok(())
}

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
    fn fill_then_verify_round_trips_across_a_chunk_boundary() {
        // Fill two adjacent chunks and confirm verify accepts each at its own
        // offset — the client fills at the send offset and verifies at the
        // receive offset, so the two must agree byte-for-byte.
        let mut a = [0u8; 100];
        let mut b = [0u8; 100];
        fill_chunk(0, &mut a);
        fill_chunk(100, &mut b);
        assert_eq!(verify_chunk(0, &a), Ok(()));
        assert_eq!(verify_chunk(100, &b), Ok(()));
        // A byte from the wrong offset is rejected at that position.
        assert_eq!(verify_chunk(0, &b), Err(0));
    }

    #[test]
    fn verify_catches_a_single_corrupted_byte() {
        let mut chunk = [0u8; 64];
        fill_chunk(500, &mut chunk);
        chunk[37] ^= 0x01;
        assert_eq!(verify_chunk(500, &chunk), Err(37));
    }

    #[test]
    fn verify_catches_a_reordered_chunk() {
        // Swapping two bytes (a reorder) is caught at the first moved byte.
        let mut chunk = [0u8; 16];
        fill_chunk(9, &mut chunk);
        chunk.swap(2, 11);
        assert_eq!(verify_chunk(9, &chunk), Err(2));
    }

    #[test]
    fn command_is_the_expected_word() {
        assert_eq!(COMMAND, "tcpecho");
        assert!(PASS_MARKER.starts_with("TCPECHO"));
    }
}
