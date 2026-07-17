//! Deterministic fuzz-style integration test for the journal persistence
//! engine ([`tairix_log::Journal`]).
//!
//! The journal turns an *untrusted* caller's admitted record into durable,
//! hash-chained on-disk segments, rotating and sealing as it goes. Every input
//! that reaches it — the requested stream, the message and field bytes, the
//! per-CPU sequence, the early-boot ring contents — is attacker-influenced, so
//! the engine must never panic and must never emit a segment that fails
//! verification. This harness drives [`Journal::commit`] and
//! [`Journal::import_boot`] on pseudo-random inputs and, after flushing,
//! asserts every persisted segment verifies (checksums, hash chain, and seal
//! where required) and that a user origin never lands on a privileged stream.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `tairix_fuzzseed` seam (one definition).

use std::cell::RefCell;
use std::rc::Rc;

use tairix_abi::{
    BootId, CapabilitySummary, Duration64, Origin, ProcId, Time64, TrustDomain, WallClockReading,
    WallTimeState, BOOT_ID_LEN, ORIGIN_CONSOLE_NONE, PROC_ID_LEN,
};
use tairix_log::journal::{Journal, SegmentStore};
use tairix_log::{
    machine_id_hash, verify_segment, BootRing, CallerContent, LogAttestationKey, Stream,
    STREAM_COUNT,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 4_000;

const MID: [u8; 16] = [0x11; 16];

fn key() -> LogAttestationKey {
    LogAttestationKey::from_key([0x24; 32])
}

/// A store that keeps every closed segment so the harness can verify them.
#[derive(Clone, Default)]
struct CaptureStore {
    segments: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl SegmentStore for CaptureStore {
    type Error = ();
    fn store_segment(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.segments.borrow_mut().push(bytes.to_vec());
        Ok(())
    }
}

fn kernel_origin() -> Origin {
    Origin::new(
        TrustDomain::Kernel,
        0,
        0,
        1,
        ProcId::KERNEL,
        CapabilitySummary::EMPTY,
        ORIGIN_CONSOLE_NONE,
    )
}

fn user_origin(uid: u32) -> Origin {
    Origin::new(
        TrustDomain::User,
        uid,
        uid,
        42,
        ProcId::from_raw([0x5A; PROC_ID_LEN]),
        CapabilitySummary::EMPTY,
        ORIGIN_CONSOLE_NONE,
    )
}

fn maybe_stream(word: u64) -> Option<Stream> {
    match word % 7 {
        0 => None,
        1 => Some(Stream::Boot),
        2 => Some(Stream::Runtime),
        3 => Some(Stream::Debug),
        4 => Some(Stream::Security),
        5 => Some(Stream::Audit),
        _ => Some(Stream::Journal),
    }
}

/// A small pool of message bytes: empty, short, and long enough to exercise
/// dictionary promotion and the occasional over-cap rejection.
const MESSAGES: [&str; 5] = [
    "",
    "started",
    "dhcp timeout",
    "a repeated line a repeated line a repeated line",
    "the quick brown fox jumped over the lazy dog many many times over",
];

fn one_round(rng: &mut tairix_fuzzseed::Lcg) {
    let store = CaptureStore::default();
    let sink = store.segments.clone();

    // Small buffers make rotation frequent; each still holds a single record.
    let mut b: [Vec<u8>; STREAM_COUNT] = core::array::from_fn(|_| vec![0u8; 700]);
    let [b0, b1, b2, b3, b4, b5] = &mut b;
    let bufs: [&mut [u8]; STREAM_COUNT] = [
        b0.as_mut_slice(),
        b1.as_mut_slice(),
        b2.as_mut_slice(),
        b3.as_mut_slice(),
        b4.as_mut_slice(),
        b5.as_mut_slice(),
    ];
    let boot = BootId::from_raw([0x5A; BOOT_ID_LEN]);
    let mut journal = Journal::new(
        store,
        machine_id_hash(&MID),
        boot,
        Some(key()),
        kernel_origin(),
        bufs,
    );

    let scratch = &mut [0u8; 8192];
    let records = rng.next_u64() % 24;
    for _ in 0..records {
        let w = rng.next_u64();
        let from_user = w & 1 == 0;
        let origin = if from_user {
            user_origin(u32::try_from((w >> 8) % 4).unwrap_or(0))
        } else {
            kernel_origin()
        };
        let requested = maybe_stream(w >> 1);
        let msg = MESSAGES[(w >> 3) as usize % MESSAGES.len()];
        let adm = journal.admit(&origin, Some("mem"), requested, None, None);
        // A user origin must never be admitted to a privileged stream.
        if from_user {
            assert!(!adm.stream().requires_trusted_emitter());
        }
        let caller = CallerContent {
            level: None,
            component: None,
            tag: None,
            event_id: None,
            requested_source: None,
            requested_stream: requested,
            message: msg,
        };
        let cpu = u32::try_from((w >> 16) % 4).unwrap_or(0);
        let cpu_seq = w >> 20;
        // The commit may reject an over-cap record; it must never panic.
        let _ = journal.commit(
            &adm,
            cpu,
            cpu_seq,
            Duration64::from_secs(i64::from(cpu)),
            WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
            caller,
            &[],
            scratch,
        );
    }

    // Occasionally import an early-boot ring, which may report a loss.
    if rng.next_u64() & 1 == 0 {
        let mut ring_buf = [0u8; 256];
        if let Ok(mut ring) = BootRing::new(
            &mut ring_buf,
            u32::try_from(rng.next_u64() % 4).unwrap_or(0),
        ) {
            let pushes = rng.next_u64() % 60;
            for seq in 0..pushes {
                let _ = ring.push(
                    seq,
                    Duration64::from_secs(i64::try_from(seq).unwrap_or(0)),
                    b"early boot line",
                );
            }
            let _ = journal.import_boot(
                &mut ring,
                Duration64::from_secs(100),
                WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
                scratch,
            );
        }
    }

    // Flushing closes every open segment; a full flush must succeed with the
    // seal key present.
    journal.flush().expect("flush");

    // Every persisted segment must verify: checksums, hash chain, and the seal
    // on audit/security streams.
    for seg in sink.borrow().iter() {
        verify_segment(seg, Some(&key())).expect("persisted segment verifies");
    }
}

#[test]
fn journal_never_panics_and_persists_verifiable_segments() {
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "journal_never_panics_and_persists_verifiable_segments",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            one_round(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
