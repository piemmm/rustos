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

use rustos_log::Stream;

/// The machine-wide log root. `/System/Logs` is one of the only writable paths
/// beneath the read-only `/System`, mounted `nosuid,nodev,noexec`.
pub const LOGS_ROOT: &str = "/System/Logs";

/// The per-stream subdirectory name.
///
/// This reuses the stream's canonical label (the same bytes fed to the stream
/// genesis), so the on-disk directory name and the integrity label are one
/// definition and cannot drift.
#[must_use]
pub fn stream_dir(stream: Stream) -> &'static str {
    // `genesis_label` is a fixed ASCII label per closed stream, so it is always
    // valid UTF-8; the fallback is unreachable and keeps this total.
    core::str::from_utf8(stream.genesis_label()).unwrap_or("unknown")
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
    use super::{segment_path, stream_dir, stream_directory};
    use rustos_log::Stream;

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
