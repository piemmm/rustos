//! Deterministic fuzz harness for the sealed scope's on-disk records.
//!
//! Both records are read from the volume before a single byte of either is
//! believed, and the sealed document is the one record whose contents an
//! attacker who reached the store tree would most want to forge. Invariants,
//! for arbitrary bytes:
//!
//! 1. `MasterSecret::decode` never panics, and accepts nothing but a record it
//!    wrote for that exact account.
//! 2. `open_document` never panics, and accepts nothing but a record sealed
//!    under that exact key — no input is ever answered as an *empty* vault,
//!    which is the confusion the sealed scope must never make.
//! 3. A sealed record round-trips: seal then open recovers every secret.
//! 4. Any single-byte mutation of a sealed record, and any truncation of one,
//!    is refused.
//! 5. Two seals never repeat a nonce, whatever the document.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::Document;
use tairix_confd::vault::{
    open_document, seal_document, Entropy, MasterSecret, VaultError, VaultKey, MASTER_SECRET_LEN,
    VAULT_HEADER_LEN,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 2_000;

/// Lehmer-style LCG — deterministic, and the same generator the sibling
/// harnesses use so a failure reproduces one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    /// The stream's high byte — an LCG's low bits are its worst, so a
    /// byte-at-a-time draw takes from the top.
    fn byte(&mut self) -> u8 {
        self.next_u64().to_le_bytes()[7]
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % bound as u64).expect("a bounded index fits")
    }
}

/// An [`Entropy`] source driven by the harness's own stream, so a sealed record
/// is reproducible from the logged seed and a nonce still differs per draw.
struct StreamEntropy<'a>(&'a mut Lcg);

impl Entropy for StreamEntropy<'_> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        for byte in out.iter_mut() {
            *byte = self.0.byte();
        }
        Ok(())
    }
}

/// Keys and values mixed short and long, plain and needing quotes, so a sealed
/// document walks the format's own accept/reject boundary too.
const KEYS: &[&str] = &[
    "imap.password",
    "smtp.password",
    "token",
    "a.b.c.d",
    "recent.0",
];
const VALUES: &[&str] = &[
    "hunter2",
    "",
    "  leading and trailing  ",
    "a # not a comment",
    "with \"quotes\" and \\ backslash",
    "line\nbreak",
];

/// A publisher identity from `tag`.
fn publisher(tag: u8) -> PublisherId {
    // The all-zero identity is the "no publisher" sentinel and cannot name an
    // application, so a drawn tag of zero becomes one.
    PublisherId::from_raw([tag.max(1); PUBLISHER_ID_LEN])
}

/// An application identity drawn from the stream.
fn identity(rng: &mut Lcg) -> AppIdentity {
    const IDS: &[&str] = &["os.tairix.mail", "os.tairix.terminal", "org.pty.widgets"];
    AppIdentity::new(IDS[rng.below(IDS.len())], publisher(rng.byte())).expect("a legal identity")
}

/// A vault key drawn from the stream.
fn key(rng: &mut Lcg) -> VaultKey {
    let app = identity(rng);
    let master = {
        let mut source = StreamEntropy(rng);
        MasterSecret::draw(&mut source).expect("the stream always answers")
    };
    master.app_key(&app)
}

/// A document of drawn secrets.
fn document(rng: &mut Lcg) -> Document {
    let mut document = Document::new();
    for _ in 0..rng.below(6) {
        let _ = document.set(KEYS[rng.below(KEYS.len())], VALUES[rng.below(VALUES.len())]);
    }
    document
}

/// A refusal is a refusal; the one thing no input may produce is an *opened*
/// document, because an application must never read a forged or damaged vault
/// as its own — or as an empty one.
fn assert_refused(outcome: Result<Document, VaultError>, what: &str) {
    match outcome {
        Ok(document) => panic!(
            "{what} opened as a vault of {} settings",
            document.settings().count()
        ),
        Err(VaultError::VaultMalformed | VaultError::VaultUnsealFailed) => {}
        Err(other) => panic!("{what} refused with {other:?}, which is not a record refusal"),
    }
}

#[test]
fn arbitrary_bytes_are_never_a_master_secret_record() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_bytes_are_never_a_master_secret_record",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let uid = rng.byte().into();
            let len = rng.below(MasterSecret::WIRE_LEN * 2);
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                *byte = rng.byte();
            }
            // Drawn bytes: overwhelmingly not a record, and never a panic.
            let _ = MasterSecret::decode(&bytes, uid);

            // A genuine record, then every single-byte mutation of it: the
            // record decodes and no mutation of a load-bearing byte does.
            let genuine = {
                let mut source = StreamEntropy(&mut rng);
                MasterSecret::draw(&mut source).expect("the stream always answers")
            }
            .encode(uid);
            assert!(
                MasterSecret::decode(&genuine, uid).is_some(),
                "a record it wrote must decode"
            );
            assert!(
                MasterSecret::decode(&genuine, uid ^ 1).is_none(),
                "and must not decode for another account"
            );
            let at = rng.below(genuine.len());
            let mut altered = genuine;
            altered[at] ^= 1 << (rng.below(8));
            // Only the secret's own bytes may still decode: every byte of the
            // header and the account binding ahead of them is load-bearing.
            if at < MasterSecret::WIRE_LEN - MASTER_SECRET_LEN {
                assert!(
                    MasterSecret::decode(&altered, uid).is_none(),
                    "byte {at} of the header is not load-bearing"
                );
            }
            let short = rng.below(MasterSecret::WIRE_LEN);
            assert!(
                MasterSecret::decode(&altered[..short], uid).is_none(),
                "a record of the wrong length is not a record"
            );
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn arbitrary_bytes_are_never_an_openable_vault() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_bytes_are_never_an_openable_vault",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let opener = key(&mut rng);
            let len = rng.below(VAULT_HEADER_LEN * 3);
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                *byte = rng.byte();
            }
            assert_refused(open_document(&opener, &bytes), "drawn bytes");
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn a_sealed_document_round_trips_and_tolerates_no_mutation() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "a_sealed_document_round_trips_and_tolerates_no_mutation",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let opener = key(&mut rng);
            let plain = document(&mut rng);
            let record = {
                let mut source = StreamEntropy(&mut rng);
                seal_document(&opener, &mut source, &plain).expect("the stream always answers")
            };

            let plain_again =
                open_document(&opener, &record).expect("a record it sealed must open");
            for setting in plain.settings() {
                assert_eq!(
                    plain_again.get(setting.key),
                    Some(setting.value),
                    "a sealed secret must come back exactly"
                );
            }
            assert_eq!(
                plain_again.settings().count(),
                plain.settings().count(),
                "and nothing else may come back with it"
            );

            let at = rng.below(record.len());
            let mut altered = record.clone();
            altered[at] ^= 1 << (rng.below(8));
            assert_refused(open_document(&opener, &altered), "a one-bit mutation");

            let short = rng.below(record.len());
            assert_refused(open_document(&opener, &record[..short]), "a truncation");

            let mut extended = record.clone();
            extended.push(rng.byte());
            assert_refused(open_document(&opener, &extended), "an extension");

            // A second seal of the same document under the same key must not
            // repeat the nonce: reusing a `(key, nonce)` pair under this AEAD
            // is catastrophic.
            let again = {
                let mut source = StreamEntropy(&mut rng);
                seal_document(&opener, &mut source, &plain).expect("the stream always answers")
            };
            assert_ne!(
                record[..VAULT_HEADER_LEN],
                again[..VAULT_HEADER_LEN],
                "two seals must not share a nonce"
            );
            assert_refused(
                open_document(&key(&mut rng), &record),
                "another application's key",
            );
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
