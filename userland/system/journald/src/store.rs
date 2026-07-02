//! Filesystem placement for persisted segments.
//!
//! The journal hands each closed segment to a [`SegmentStore`](rustos_log::SegmentStore).
//! The production sink writes every segment as its own file under
//! `/System/Logs/<stream>/`, named by the segment's own id, so segments are
//! immutable append-only files that rotation never rewrites (SYSLOG §6, §10).
//!
//! The path derivation is pure and lives here so it is host-tested independent
//! of any syscall; the sink that actually opens and writes the file over the
//! `rustos-rt` filesystem wrappers is the service binary's concern (the staged
//! follow-on that binds the ingress endpoint and drives the dispatch core over
//! real syscalls — SYSLOG §15).

use alloc::string::String;

use rustos_log::{SegmentError, SegmentReader, Stream};

/// The machine-wide log root. `/System/Logs` is one of the only writable paths
/// beneath the read-only `/System`, mounted `nosuid,nodev,noexec`.
pub const LOGS_ROOT: &str = "/System/Logs";

/// Absolute path of the per-installation **log-attestation key** the journal
/// principal reads at startup to seal `audit`/`security` segments
/// (`PREREQUISITES.md` P-E). A secret, owner-read/write-only under the
/// read-only `/System/Security/Keys` (the installer/image writes it there).
pub const LOG_ATTESTATION_KEY_PATH: &str = "/System/Security/Keys/LogAttestation";

/// Absolute path of the per-installation **machine-id** the journal reads at
/// startup to bind each stream's hash-chain genesis to this installation
/// (`AGENTS.md` §16.2; SYSLOG §7.1). Non-secret, world-readable public
/// identity (the RustOS equivalent of `/etc/machine-id`); the installer/image
/// provisions it, so a missing file means an unprovisioned system.
pub const MACHINE_ID_PATH: &str = "/System/Security/MachineId";

/// Derive where a closed segment image belongs on disk — its containing
/// per-stream directory and its `/System/Logs/<stream>/<id>.seg` file path —
/// by parsing the segment's own self-checksummed header for the stream and
/// segment id, never a caller-supplied placement.
///
/// This is the pure half of the filesystem sink: it is host-testable without
/// any syscall, and the freestanding `Run` binary's `SegmentStore` only adds
/// the `fs_*` calls that create the directory and write the bytes to the
/// returned `(directory, file)` paths.
///
/// # Errors
///
/// The [`SegmentError`] from [`SegmentReader::open`] if `bytes` is not a valid
/// segment image (a truncated or corrupt header) — fail closed, never writing
/// a segment to a guessed path.
pub fn segment_placement_for(bytes: &[u8]) -> Result<(String, String), SegmentError> {
    let reader = SegmentReader::open(bytes)?;
    let header = reader.header();
    Ok((
        stream_directory(header.stream),
        segment_path(header.stream, header.segment_id),
    ))
}

/// The per-stream subdirectory name.
///
/// This is the stream's canonical name (the same spelling fed to the stream
/// genesis and used by the rich renderers), so the on-disk directory name and
/// the integrity label are one definition and cannot drift.
#[must_use]
pub fn stream_dir(stream: Stream) -> &'static str {
    stream.name()
}

/// The path a segment of `stream` with id `segment_id` is written to:
/// `/System/Logs/<stream>/<segment_id>.seg`, the id zero-padded to 16 hex
/// digits so a lexical directory listing is also chronological.
#[must_use]
pub fn segment_path(stream: Stream, segment_id: u64) -> String {
    alloc::format!("{LOGS_ROOT}/{}/{segment_id:016x}.seg", stream_dir(stream))
}

/// The directory a stream's segments live in: `/System/Logs/<stream>`.
#[must_use]
pub fn stream_directory(stream: Stream) -> String {
    alloc::format!("{LOGS_ROOT}/{}", stream_dir(stream))
}

#[cfg(test)]
mod tests {
    use super::{segment_path, segment_placement_for, stream_dir, stream_directory};
    use rustos_abi::time::{Duration64, Time64, WallClockReading, WallTimeState};
    use rustos_abi::{BootId, BOOT_ID_LEN};
    use rustos_log::{machine_id_hash, SegmentHeader, SegmentWriter, Stream};

    /// Build a minimal (record-free) closed segment image for `stream`/`id`,
    /// so the placement derivation can be exercised on a real header.
    fn empty_segment(stream: Stream, id: u64) -> alloc::vec::Vec<u8> {
        let header = SegmentHeader {
            stream,
            segment_id: id,
            machine_id_hash: machine_id_hash(&[0x33; 16]),
            boot_id: BootId::from_raw([0x5A; BOOT_ID_LEN]),
            first_seq: 0,
            prev_segment_hash: [0u8; 32],
            creation_monotonic: Duration64::from_secs(1),
            creation_wall: WallClockReading::new(Time64::from_secs(1), WallTimeState::Trusted),
        };
        let mut buf = alloc::vec![0u8; 4096];
        // Runtime needs no seal; a record-free segment closes immediately.
        let writer = SegmentWriter::begin(&mut buf, &header).expect("begin");
        let finished = writer.finish(None).expect("finish");
        // `finished` borrows `buf`; copy the closed image out before it drops.
        finished.buf[..finished.len].to_vec()
    }

    #[test]
    fn segment_placement_reads_the_dir_and_file_from_the_header() {
        let seg = empty_segment(Stream::Runtime, 42);
        let (dir, file) = segment_placement_for(&seg).expect("valid segment");
        assert_eq!(dir, "/System/Logs/runtime");
        assert_eq!(file, "/System/Logs/runtime/000000000000002a.seg");
    }

    #[test]
    fn segment_placement_fails_closed_on_a_corrupt_header() {
        // A too-short blob cannot carry a whole header, so placement fails
        // closed rather than guessing a path (the exact `SegmentError` variant
        // is the reader's concern; here it is enough that no path is minted).
        assert!(segment_placement_for(&[0u8; 8]).is_err());
    }

    #[test]
    fn stream_dir_matches_the_canonical_label() {
        assert_eq!(stream_dir(Stream::Boot), "boot");
        assert_eq!(stream_dir(Stream::Runtime), "runtime");
        assert_eq!(stream_dir(Stream::Security), "security");
        assert_eq!(stream_dir(Stream::Audit), "audit");
        assert_eq!(stream_dir(Stream::Journal), "journal");
    }

    #[test]
    fn segment_path_is_padded_and_rooted() {
        assert_eq!(
            segment_path(Stream::Runtime, 3),
            "/System/Logs/runtime/0000000000000003.seg"
        );
        assert_eq!(stream_directory(Stream::Audit), "/System/Logs/audit");
    }

    #[test]
    fn segment_paths_sort_chronologically() {
        // Zero-padding makes a lexical sort agree with the numeric id order.
        let a = segment_path(Stream::Runtime, 9);
        let b = segment_path(Stream::Runtime, 10);
        assert!(a < b);
    }
}
