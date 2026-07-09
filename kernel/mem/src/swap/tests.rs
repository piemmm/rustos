//! Unit tests for the encrypted-swap layer.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// Deterministic, non-cryptographic entropy stand-in for tests: fills with
/// a counter so each call is distinct (enough to exercise the salt path).
struct CountingEntropy {
    next: u8,
}

impl CountingEntropy {
    fn new() -> Self {
        Self { next: 1 }
    }
}

impl EntropySource for CountingEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

/// Entropy source that always fails, to exercise the fail-closed path.
struct DeadEntropy;

impl EntropySource for DeadEntropy {
    fn fill(&mut self, _out: &mut [u8]) -> Result<(), SealError> {
        Err(SealError::Entropy)
    }
}

/// Byte offset of `slot`'s record (also the byte length for `slot`
/// records). Checked conversion so the cast lint stays satisfied.
fn record_base(slot: u64) -> usize {
    usize::try_from(slot).expect("slot index fits usize in tests") * SWAP_RECORD_LEN
}

/// In-memory swap device of `slots` records.
struct MockBackend {
    storage: Vec<u8>,
    slots: u64,
    read_faults: bool,
}

impl MockBackend {
    fn new(slots: u64) -> Self {
        Self {
            storage: vec![0u8; record_base(slots)],
            slots,
            read_faults: false,
        }
    }

    fn raw(&self, slot: u64) -> &[u8] {
        let base = record_base(slot);
        &self.storage[base..base + SWAP_RECORD_LEN]
    }

    fn flip_byte(&mut self, slot: u64, offset: usize) {
        let base = record_base(slot);
        self.storage[base + offset] ^= 0x01;
    }

    fn copy_slot(&mut self, from: u64, to: u64) {
        let src = self.raw(from).to_vec();
        let base = record_base(to);
        self.storage[base..base + SWAP_RECORD_LEN].copy_from_slice(&src);
    }
}

impl SwapBackend for MockBackend {
    fn slot_count(&self) -> u64 {
        self.slots
    }

    fn write_record(&mut self, slot: u64, record: &[u8]) -> Result<(), SwapError> {
        if record.len() != SWAP_RECORD_LEN || slot >= self.slots {
            return Err(SwapError::Backend);
        }
        let base = record_base(slot);
        self.storage[base..base + SWAP_RECORD_LEN].copy_from_slice(record);
        Ok(())
    }

    fn read_record(&self, slot: u64, record: &mut [u8]) -> Result<(), SwapError> {
        if self.read_faults || record.len() != SWAP_RECORD_LEN || slot >= self.slots {
            return Err(SwapError::Backend);
        }
        record.copy_from_slice(self.raw(slot));
        Ok(())
    }
}

fn activate(slots: u64) -> EncryptedSwap<MockBackend> {
    let mut ent = CountingEntropy::new();
    let key = SealKey::generate(&mut ent).expect("key");
    EncryptedSwap::activate(MockBackend::new(slots), key, &mut ent).expect("activate")
}

fn page(fill: u8) -> SwapPage {
    [fill; PAGE_SIZE]
}

#[test]
fn store_then_load_round_trips() {
    let mut swap = activate(4);
    let original = page(0xAB);
    swap.store(2, &original).expect("store");
    let mut out = page(0x00);
    swap.load(2, &mut out).expect("load");
    assert_eq!(out, original);
}

#[test]
fn record_holds_no_plaintext() {
    let mut swap = activate(2);
    let original = page(0x5A);
    swap.store(0, &original).expect("store");
    // The ciphertext region must differ from the plaintext page.
    let ciphertext = &swap.backend.raw(0)[CIPHERTEXT_OFFSET..];
    assert_ne!(ciphertext, &original[..], "swap record leaked plaintext");
}

#[test]
fn distinct_nonces_produce_distinct_ciphertext() {
    let mut swap = activate(2);
    let p = page(0x11);
    swap.store(0, &p).expect("store 0");
    swap.store(1, &p).expect("store 1");
    // Same plaintext, different slot/nonce ⇒ different stored records.
    assert_ne!(swap.backend.raw(0), swap.backend.raw(1));
}

#[test]
fn tampered_ciphertext_is_rejected_and_buffer_zeroed() {
    let mut swap = activate(1);
    swap.store(0, &page(0xCC)).expect("store");
    swap.backend.flip_byte(0, CIPHERTEXT_OFFSET + 10);
    let mut out = page(0x77);
    assert_eq!(swap.load(0, &mut out), Err(SwapError::Authentication));
    assert!(
        out.iter().all(|b| *b == 0),
        "buffer must be zeroed on failure"
    );
}

#[test]
fn tampered_tag_is_rejected() {
    let mut swap = activate(1);
    swap.store(0, &page(0xCC)).expect("store");
    swap.backend.flip_byte(0, TAG_OFFSET);
    let mut out = page(0x00);
    assert_eq!(swap.load(0, &mut out), Err(SwapError::Authentication));
}

#[test]
fn tampered_nonce_is_rejected() {
    let mut swap = activate(1);
    swap.store(0, &page(0xCC)).expect("store");
    swap.backend.flip_byte(0, NONCE_OFFSET);
    let mut out = page(0x00);
    assert_eq!(swap.load(0, &mut out), Err(SwapError::Authentication));
}

#[test]
fn relocated_record_fails_authentication() {
    let mut swap = activate(2);
    swap.store(0, &page(0xDD)).expect("store");
    // Move slot 0's record verbatim into slot 1; the slot-index AAD no
    // longer matches, so authentication must fail (§ relocation defence).
    swap.backend.copy_slot(0, 1);
    let mut out = page(0x00);
    assert_eq!(swap.load(1, &mut out), Err(SwapError::Authentication));
}

#[test]
fn store_out_of_range_fails_closed() {
    let mut swap = activate(2);
    assert_eq!(swap.store(2, &page(0x01)), Err(SwapError::SlotOutOfRange));
    assert_eq!(swap.store(99, &page(0x01)), Err(SwapError::SlotOutOfRange));
}

#[test]
fn load_out_of_range_fails_closed() {
    let swap = activate(2);
    let mut out = page(0x00);
    assert_eq!(swap.load(2, &mut out), Err(SwapError::SlotOutOfRange));
}

#[test]
fn backend_read_fault_zeroes_buffer() {
    let mut swap = activate(1);
    swap.store(0, &page(0xEE)).expect("store");
    swap.backend.read_faults = true;
    let mut out = page(0x99);
    assert_eq!(swap.load(0, &mut out), Err(SwapError::Backend));
    assert!(
        out.iter().all(|b| *b == 0),
        "buffer must be zeroed on backend fault"
    );
}

#[test]
fn nonce_counter_exhaustion_fails_closed() {
    let mut swap = activate(1);
    // Drive the counter to the brink; the next nonce would overflow.
    swap.nonces = NonceSequence::with_counter([0u8; 4], u64::MAX);
    assert_eq!(swap.store(0, &page(0x01)), Err(SwapError::NonceExhausted));
}

#[test]
fn activation_fails_closed_on_dead_entropy() {
    let mut good = CountingEntropy::new();
    let key = SealKey::generate(&mut good).expect("key");
    let mut dead = DeadEntropy;
    let result = EncryptedSwap::activate(MockBackend::new(1), key, &mut dead);
    assert!(matches!(result, Err(SwapError::Entropy)));
}

#[test]
fn slot_count_is_reported() {
    let swap = activate(7);
    assert_eq!(swap.slot_count(), 7);
}

#[test]
fn record_length_matches_layout() {
    assert_eq!(SWAP_RECORD_LEN, AEAD_NONCE_LEN + AEAD_TAG_LEN + PAGE_SIZE);
    assert_eq!(CIPHERTEXT_OFFSET, AEAD_NONCE_LEN + AEAD_TAG_LEN);
}
