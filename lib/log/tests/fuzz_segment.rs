//! Deterministic fuzz-style integration test for the segment decoder.
//!
//! A log segment under `/System/Logs` is attacker-influenced (a compromised
//! journal, a tampered or torn-on-power-loss file, a volume lifted from
//! another machine), so every segment-reading path — [`rustos_log::SegmentHeader::parse`],
//! the forward-scanning [`rustos_log::SegmentReader`], and the full
//! [`rustos_log::verify_segment`] — must refuse malformed bytes cleanly and
//! never panic. This harness drives all three on both pseudo-random bytes and
//! single-byte mutations of a genuine segment (so the checksum, chain, footer,
//! and seal paths are actually reached), asserting only that they never panic
//! and that a forward scan always terminates.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::{BootId, Duration64, Time64, WallClockReading, WallTimeState, BOOT_ID_LEN};
use rustos_log::{
    machine_id_hash, stream_genesis, LogAttestationKey, SegmentHeader, SegmentReader,
    SegmentWriter, Stream, MAX_RECORD_PAYLOAD,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

const MID: [u8; 16] = [0x11; 16];

fn key() -> LogAttestationKey {
    LogAttestationKey::from_key([0x24; 32])
}

/// Build a genuine sealed audit segment so mutations reach every check.
fn base_segment(buf: &mut [u8]) -> usize {
    let boot = BootId::from_raw([0x5A; BOOT_ID_LEN]);
    let header = SegmentHeader {
        stream: Stream::Audit,
        segment_id: 3,
        machine_id_hash: machine_id_hash(&MID),
        boot_id: boot,
        first_seq: 42,
        prev_segment_hash: stream_genesis(
            &machine_id_hash(&MID),
            Stream::Audit.genesis_label(),
            &boot,
        ),
        creation_monotonic: Duration64::from_secs(1),
        creation_wall: WallClockReading::new(
            Time64::from_secs(1_700_000_000),
            WallTimeState::Trusted,
        ),
    };
    let mut w = SegmentWriter::begin(buf, &header).expect("begin");
    w.append_record(0, Duration64::from_secs(10), b"login denied")
        .expect("append");
    w.append_record(1, Duration64::from_secs(11), b"policy changed")
        .expect("append");
    w.finish(Some(&key())).expect("finish").len
}

/// Drive every segment-reading path; the contract is "must not panic".
fn exercise(bytes: &[u8], k: &LogAttestationKey) {
    let _ = SegmentHeader::parse(bytes);
    if let Ok(mut reader) = SegmentReader::open(bytes) {
        let mut count = 0usize;
        for block in &mut reader {
            assert!(block.payload.len() <= MAX_RECORD_PAYLOAD);
            count += 1;
            // Each record consumes at least the fixed prefix, so a scan of
            // `bytes` can never yield more records than there are bytes.
            assert!(count <= bytes.len(), "forward scan failed to terminate");
        }
        let _ = reader.end();
        let _ = reader.head_hash();
    }
    let _ = rustos_log::verify_segment(bytes, None);
    let _ = rustos_log::verify_segment(bytes, Some(k));
}

#[test]
fn random_and_mutated_segments_never_panic() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "random_and_mutated_segments_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let k = key();

    let mut base = [0u8; 1024];
    let base_len = base_segment(&mut base);

    let mut buf = [0u8; 1024];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            if i % 2 == 0 {
                // Pure random bytes of a random length.
                let size = ((rng.next_u64() & 0x3FF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                exercise(&buf[..size], &k);
            } else {
                // A genuine segment with a handful of byte flips.
                buf[..base_len].copy_from_slice(&base[..base_len]);
                let flips = (rng.next_u64() % 4) + 1;
                for _ in 0..flips {
                    let pos = ((rng.next_u64() & 0x3FF) as usize) % base_len;
                    buf[pos] ^= rng.next_u64().to_le_bytes()[0] | 1;
                }
                exercise(&buf[..base_len], &k);
            }
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
