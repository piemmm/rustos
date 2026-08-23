//! Tests for the sealed scope's records and key hierarchy: the master secret,
//! the per-application derivation, and the sealed document.

use alloc::vec::Vec;

use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::Document;

use super::{
    open_document, seal_document, Entropy, MasterSecret, VaultError, MASTER_SECRET_LEN,
    VAULT_HEADER_LEN,
};

/// The account the fixtures seal for.
const UID: u32 = 1000;

/// A deterministic entropy source: every draw is a counting sequence, so a
/// sealed record is reproducible and a nonce is still distinct per draw.
pub struct CountingEntropy {
    next: u8,
    /// When set, every draw refuses — the not-yet-seeded generator.
    refuse: Option<Errno>,
    /// When set, every draw answers all zeroes — a generator that is answering
    /// but is broken.
    zeroed: bool,
}

impl CountingEntropy {
    /// A source whose draws count upward from `first`.
    pub const fn new(first: u8) -> Self {
        Self {
            next: first,
            refuse: None,
            zeroed: false,
        }
    }

    /// A source that refuses every draw.
    pub const fn refusing(err: Errno) -> Self {
        Self {
            next: 0,
            refuse: Some(err),
            zeroed: false,
        }
    }

    /// A source that answers every draw with zeroes.
    pub const fn broken() -> Self {
        Self {
            next: 0,
            refuse: None,
            zeroed: true,
        }
    }
}

impl Entropy for CountingEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
        if let Some(err) = self.refuse {
            return Err(err);
        }
        for byte in out.iter_mut() {
            *byte = if self.zeroed { 0 } else { self.next };
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

/// The identity `bundle_id` runs under, published by `tag`.
fn identity(bundle_id: &str, tag: u8) -> AppIdentity {
    AppIdentity::new(bundle_id, PublisherId::from_raw([tag; PUBLISHER_ID_LEN]))
        .expect("a legal identity")
}

/// A master secret drawn from a counting source.
fn master() -> MasterSecret {
    MasterSecret::draw(&mut CountingEntropy::new(1)).expect("a draw succeeds")
}

/// A document with one secret in it.
fn secrets() -> Document {
    let mut document = Document::new();
    document
        .set("imap.password", "correct horse battery staple")
        .expect("a legal setting");
    document
}

#[test]
fn a_master_secret_record_round_trips() {
    let master = master();
    let bytes = master.encode(UID);
    assert_eq!(bytes.len(), MasterSecret::WIRE_LEN);
    let decoded = MasterSecret::decode(&bytes, UID).expect("a record it wrote decodes");
    // The bytes never leave the type, so equality is proved through the one
    // thing they are used for: the derived key.
    let app = identity("os.tairix.mail", 7);
    assert_eq!(
        seal_and_open(&decoded.app_key(&app), &master.app_key(&app)),
        Ok(())
    );
}

#[test]
fn a_master_secret_record_is_bound_to_its_account() {
    let bytes = master().encode(UID);
    assert!(MasterSecret::decode(&bytes, UID).is_some());
    // A record copied into another account's home attests nothing there, so
    // two accounts can never be given one key hierarchy by a file move.
    assert!(MasterSecret::decode(&bytes, UID + 1).is_none());
    assert!(MasterSecret::decode(&bytes, 0).is_none());
}

#[test]
fn anything_that_is_not_exactly_a_master_secret_record_is_refused() {
    let bytes = master().encode(UID);

    assert!(MasterSecret::decode(&bytes[..bytes.len() - 1], UID).is_none());
    let mut long = Vec::from(&bytes[..]);
    long.push(0);
    assert!(MasterSecret::decode(&long, UID).is_none());
    assert!(MasterSecret::decode(&[], UID).is_none());

    let mut wrong = bytes;
    wrong[0] ^= 0xFF;
    assert!(MasterSecret::decode(&wrong, UID).is_none());

    let mut future = bytes;
    future[4] = 2;
    assert!(MasterSecret::decode(&future, UID).is_none());

    let mut dirty = bytes;
    dirty[super::HEADER_LEN - 1] = 1;
    assert!(MasterSecret::decode(&dirty, UID).is_none());
}

/// The likeliest corruption shape — a file allocated but never written — must
/// not read as a usable key. It would otherwise give every account with a
/// zeroed record the same key hierarchy.
#[test]
fn a_zeroed_master_secret_record_is_not_a_record() {
    assert!(MasterSecret::decode(&[0u8; MasterSecret::WIRE_LEN], 0).is_none());
    let mut zeroed_secret = master().encode(UID);
    for byte in &mut zeroed_secret[super::MASTER_SECRET_OFFSET..] {
        *byte = 0;
    }
    assert!(MasterSecret::decode(&zeroed_secret, UID).is_none());
}

#[test]
fn a_draw_that_fails_or_answers_zeroes_yields_no_master_secret() {
    assert_eq!(
        MasterSecret::draw(&mut CountingEntropy::refusing(Errno::EntropyNotReady)).err(),
        Some(VaultError::EntropyUnavailable)
    );
    // A generator that answers, but answers zeroes, is broken rather than
    // unlucky: a working CSPRNG will not do it in the lifetime of the universe.
    assert_eq!(
        MasterSecret::draw(&mut CountingEntropy::broken()).err(),
        Some(VaultError::EntropyUnavailable)
    );
}

#[test]
fn a_sealed_document_round_trips() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&key, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    assert!(record.len() > VAULT_HEADER_LEN);
    // The plaintext must not be in the record.
    assert!(!window_contains(&record, b"correct horse"));
    let opened = open_document(&key, &record).expect("opens");
    assert_eq!(
        opened.get("imap.password"),
        Some("correct horse battery staple")
    );
}

#[test]
fn an_empty_sealed_document_round_trips() {
    // An application that removed its last secret still has a vault, and it
    // must open as the empty document rather than as a malformed record.
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record =
        seal_document(&key, &mut CountingEntropy::new(3), &Document::new()).expect("seals");
    assert_eq!(record.len(), VAULT_HEADER_LEN);
    assert_eq!(
        open_document(&key, &record)
            .expect("opens")
            .settings()
            .count(),
        0
    );
}

#[test]
fn every_seal_draws_a_fresh_nonce() {
    // Reusing a `(key, nonce)` pair under ChaCha20-Poly1305 is catastrophic,
    // so two seals of the same document must not produce the same record.
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let mut entropy = CountingEntropy::new(1);
    let first = seal_document(&key, &mut entropy, &secrets()).expect("seals");
    let second = seal_document(&key, &mut entropy, &secrets()).expect("seals");
    assert_ne!(first, second, "two seals must not share a nonce");
    assert_ne!(
        first[super::VAULT_NONCE_OFFSET..super::VAULT_TAG_OFFSET],
        second[super::VAULT_NONCE_OFFSET..super::VAULT_TAG_OFFSET]
    );
}

#[test]
fn a_seal_with_no_entropy_writes_nothing() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    assert_eq!(
        seal_document(
            &key,
            &mut CountingEntropy::refusing(Errno::EntropyNotReady),
            &secrets()
        )
        .err(),
        Some(VaultError::EntropyUnavailable)
    );
}

#[test]
fn any_single_byte_altered_fails_authentication() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&key, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    for at in 0..record.len() {
        let mut altered = record.clone();
        altered[at] ^= 0x01;
        let outcome = open_document(&key, &altered);
        // A byte in the recognised header may make the record unrecognisable
        // rather than merely unauthentic; either way it never opens.
        assert!(
            matches!(
                outcome.err(),
                Some(VaultError::VaultUnsealFailed | VaultError::VaultMalformed)
            ),
            "byte {at} altered and the record still opened"
        );
    }
}

#[test]
fn a_truncated_record_never_opens() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&key, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    for len in 0..record.len() {
        let outcome = open_document(&key, &record[..len]);
        assert!(
            matches!(
                outcome.err(),
                Some(VaultError::VaultUnsealFailed | VaultError::VaultMalformed)
            ),
            "a record truncated to {len} bytes still opened"
        );
    }
}

#[test]
fn a_record_that_is_not_a_vault_is_refused_before_any_key_is_used() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&key, &mut CountingEntropy::new(9), &secrets()).expect("seals");

    let mut wrong_magic = record.clone();
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        open_document(&key, &wrong_magic).err(),
        Some(VaultError::VaultMalformed)
    );

    let mut future = record.clone();
    future[4] = 2;
    assert_eq!(
        open_document(&key, &future).err(),
        Some(VaultError::VaultMalformed)
    );

    let mut dirty = record;
    dirty[super::HEADER_LEN - 1] = 1;
    assert_eq!(
        open_document(&key, &dirty).err(),
        Some(VaultError::VaultMalformed)
    );

    assert_eq!(
        open_document(&key, &[]).err(),
        Some(VaultError::VaultMalformed)
    );
}

/// The record's plaintext header is authenticated as well as structurally
/// pinned. The two are belt and braces — the header is compared against the one
/// constant on the way in, so the associated data is not what catches an edited
/// one — and this proves the AEAD half is genuinely wired rather than sealing
/// under an empty associated data.
#[test]
fn the_plaintext_header_is_the_associated_data() {
    let key = master().app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&key, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    let nonce = record[super::VAULT_NONCE_OFFSET..super::VAULT_TAG_OFFSET]
        .try_into()
        .expect("a nonce-wide field");
    let tag = record[super::VAULT_TAG_OFFSET..VAULT_HEADER_LEN]
        .try_into()
        .expect("a tag-wide field");

    let mut body = Vec::from(&record[VAULT_HEADER_LEN..]);
    assert!(
        tairix_crypto::open(&key.bytes, &nonce, &super::VAULT_HEADER, &mut body, &tag).is_ok(),
        "the record's own header opens it as its associated data"
    );

    let mut body = Vec::from(&record[VAULT_HEADER_LEN..]);
    assert!(
        tairix_crypto::open(&key.bytes, &nonce, b"", &mut body, &tag).is_err(),
        "an empty associated data must not, so the prefix really is bound"
    );
}

#[test]
fn a_different_application_derives_a_different_key() {
    let master = master();
    let mine = master.app_key(&identity("os.tairix.mail", 7));
    let theirs = master.app_key(&identity("os.tairix.terminal", 7));
    let record = seal_document(&mine, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    assert_eq!(
        open_document(&theirs, &record).err(),
        Some(VaultError::VaultUnsealFailed),
        "another application's key must not open this vault"
    );
}

/// Ownership is pinned to the publisher, and so is the key: a developer
/// squatting another's bundle identifier derives a different key and cannot read
/// the vault even if the ownership pin were somehow bypassed.
#[test]
fn a_different_publisher_of_the_same_identifier_derives_a_different_key() {
    let master = master();
    let real = master.app_key(&identity("os.tairix.mail", 7));
    let squatter = master.app_key(&identity("os.tairix.mail", 8));
    let record = seal_document(&real, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    assert_eq!(
        open_document(&squatter, &record).err(),
        Some(VaultError::VaultUnsealFailed)
    );
}

#[test]
fn a_different_account_derives_a_different_key() {
    let app = identity("os.tairix.mail", 7);
    let mine = MasterSecret::draw(&mut CountingEntropy::new(1)).expect("draws");
    let theirs = MasterSecret::draw(&mut CountingEntropy::new(200)).expect("draws");
    let record = seal_document(
        &mine.app_key(&app),
        &mut CountingEntropy::new(9),
        &secrets(),
    )
    .expect("seals");
    assert_eq!(
        open_document(&theirs.app_key(&app), &record).err(),
        Some(VaultError::VaultUnsealFailed)
    );
}

/// The identifier is the *last*, variable-width field of the derivation
/// context, and every field before it is fixed width — so no two
/// (publisher, identifier) pairs can share a context by concatenating
/// differently.
#[test]
fn the_derivation_context_cannot_be_confused_between_applications() {
    let master = master();
    let app = master.app_key(&identity("os.tairix", 7));
    let other = master.app_key(&identity("os.tairix.mail", 7));
    let record = seal_document(&app, &mut CountingEntropy::new(9), &secrets()).expect("seals");
    assert_eq!(
        open_document(&other, &record).err(),
        Some(VaultError::VaultUnsealFailed)
    );
    // And the label itself is load-bearing: the context is not the bare
    // publisher-and-identifier bytes.
    let context = super::context(PublisherId::from_raw([7; PUBLISHER_ID_LEN]), "os.tairix");
    assert!(context.starts_with(super::SECRET_CONTEXT));
    assert_eq!(context[super::SECRET_CONTEXT.len()], 0);
    assert_eq!(
        context.len(),
        super::SECRET_CONTEXT.len() + 1 + PUBLISHER_ID_LEN + 9
    );
}

#[test]
fn a_master_secret_is_one_aead_key_wide() {
    assert_eq!(MASTER_SECRET_LEN, tairix_crypto::AEAD_KEY_LEN);
}

/// Seal under `writer` and open under `reader`, so a test can assert that two
/// separately obtained keys are the same key without either exposing its bytes.
fn seal_and_open(writer: &super::VaultKey, reader: &super::VaultKey) -> Result<(), VaultError> {
    let record = seal_document(writer, &mut CountingEntropy::new(9), &secrets())?;
    open_document(reader, &record).map(|_| ())
}

/// Whether `needle` appears anywhere in `haystack`.
fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Wrapping a master secret takes the caller's buffer rather than copying it.
/// An array of bytes is `Copy`, so a by-value constructor would leave a live
/// copy of the secret on every caller's stack; this is what makes the one wipe
/// cover the draw and the decode alike.
#[test]
fn wrapping_a_master_secret_consumes_the_callers_buffer() {
    let mut raw = [7u8; MASTER_SECRET_LEN];
    let master = MasterSecret::from_bytes(&mut raw).expect("a non-zero secret");
    assert_eq!(
        raw, [0u8; MASTER_SECRET_LEN],
        "the caller's copy of the secret is gone"
    );
    // And it really took the bytes it was handed, not zeroes.
    let app = identity("os.tairix.mail", 7);
    let mut same = [7u8; MASTER_SECRET_LEN];
    let other = MasterSecret::from_bytes(&mut same).expect("a non-zero secret");
    assert_eq!(
        seal_and_open(&master.app_key(&app), &other.app_key(&app)),
        Ok(())
    );

    // The refusal path wipes too, so a zeroed record leaves nothing behind
    // either.
    let mut zeroed = [0u8; MASTER_SECRET_LEN];
    assert!(MasterSecret::from_bytes(&mut zeroed).is_none());
    assert_eq!(zeroed, [0u8; MASTER_SECRET_LEN]);
}
