//! Deterministic fuzz harness for the encrypted-swap restore path.
//!
//! `EncryptedSwap::load` reads a record off a swap *device* — bytes that, in
//! the threat model of, an attacker with disk access may have
//! rewritten at will. Per ("every parser of untrusted input... has a
//! fuzz target") it is driven here against arbitrary device contents.
//!
//! TAIRiX does not pull in an external fuzz runner: a
//! deterministic, per-run-seeded PRNG drives random pages, slots, and byte-level
//! tampering against an in-memory swap device and asserts the invariants the
//! restore path must uphold no matter what is on the platter:
//!
//! 1. `load` never panics for any device contents.
//! 2. An untampered round-trip returns the exact page that was stored.
//! 3. **Fail-closed**: any tampering with a stored record
//!    — ciphertext, tag, nonce, or relocation to another slot — makes `load`
//!    return `Err`; a forgery is never accepted as plaintext.
//! 4. On any error the caller's output buffer is fully zeroed.
//!
//! The device's bytes are held behind an `Rc<RefCell<..>>` so the harness
//! retains a handle to rewrite them after the `EncryptedSwap` has taken
//! ownership of the backend — the encrypted layer exposes no plaintext path
//! of its own, by design.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed. When
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing from the *same continuing* PRNG stream until the budget elapses,
//! while the logged seed keeps any failure reproducible.

use std::cell::RefCell;
use std::rc::Rc;

use tairix_kernel_mem::swap::SWAP_RECORD_LEN;
use tairix_kernel_mem::{
    EncryptedSwap, EntropySource, SealError, SealKey, SwapBackend, SwapError, SwapPage,
};

const SMOKE_ITERATIONS: u64 = 20_000;
const PAGE_LEN: usize = core::mem::size_of::<SwapPage>();
const SLOTS: u64 = 16;

/// Byte offset of `slot`'s record (also the byte length for `slot`
/// records). Checked conversion so the cast lint stays satisfied.
fn record_base(slot: u64) -> usize {
    usize::try_from(slot).expect("slot index fits usize in tests") * SWAP_RECORD_LEN
}

/// `SWAP_RECORD_LEN` as a `u64`, via a checked conversion.
fn record_len_u64() -> u64 {
    u64::try_from(SWAP_RECORD_LEN).expect("record length fits u64")
}

/// xor-shift* PRNG. Deterministic, fast, zero-allocation.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
}

/// PRNG-seeded entropy source (test-only; not a real CSPRNG).
struct RngEntropy(Rng);
impl EntropySource for RngEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
        for b in out.iter_mut() {
            *b = self.0.byte();
        }
        Ok(())
    }
}

/// In-memory swap device sharing its byte store with the harness so the
/// harness can rewrite records after the backend has been handed away.
#[derive(Clone)]
struct MockBackend {
    storage: Rc<RefCell<Vec<u8>>>,
    slots: u64,
}
impl MockBackend {
    fn new(slots: u64) -> Self {
        Self {
            storage: Rc::new(RefCell::new(vec![0u8; record_base(slots)])),
            slots,
        }
    }
    fn tamper(&self, slot: u64, offset: usize, value: u8) {
        let base = record_base(slot) + offset;
        self.storage.borrow_mut()[base] ^= value | 1;
    }
    fn relocate(&self, from: u64, to: u64) {
        let s = record_base(from);
        let d = record_base(to);
        let mut store = self.storage.borrow_mut();
        let src = store[s..s + SWAP_RECORD_LEN].to_vec();
        store[d..d + SWAP_RECORD_LEN].copy_from_slice(&src);
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
        self.storage.borrow_mut()[base..base + SWAP_RECORD_LEN].copy_from_slice(record);
        Ok(())
    }
    fn read_record(&self, slot: u64, record: &mut [u8]) -> Result<(), SwapError> {
        if record.len() != SWAP_RECORD_LEN || slot >= self.slots {
            return Err(SwapError::Backend);
        }
        let base = record_base(slot);
        record.copy_from_slice(&self.storage.borrow()[base..base + SWAP_RECORD_LEN]);
        Ok(())
    }
}

#[test]
fn fuzz_swap_restore_is_fail_closed() {
    let mut rng = Rng::new(tairix_fuzzseed::start(
        "fuzz_swap_restore_is_fail_closed",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let device = MockBackend::new(SLOTS);
    let key = SealKey::generate(&mut RngEntropy(Rng::new(1))).expect("key");
    let mut swap = EncryptedSwap::activate(device.clone(), key, &mut RngEntropy(Rng::new(2)))
        .expect("activate");

    let mut round_trips = 0u64;
    let mut tamper_rejected = 0u64;
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let slot = rng.next_u64() % SLOTS;
            let mut page = [0u8; PAGE_LEN];
            for b in &mut page {
                *b = rng.byte();
            }

            // Store, then immediately verify the untampered round-trip.
            swap.store(slot, &page)
                .expect("store within range never fails");
            let mut out = [0u8; PAGE_LEN];
            swap.load(slot, &mut out).expect("untampered load");
            assert_eq!(out, page, "round-trip must be faithful");
            round_trips += 1;

            // Occasionally tamper and assert the restore fails closed.
            match rng.next_u64() % 4 {
                0 => {
                    let off = usize::try_from(rng.next_u64() % record_len_u64())
                        .expect("offset fits usize");
                    swap.store(slot, &page).expect("re-store");
                    device.tamper(slot, off, rng.byte());
                    let mut out = [0xAAu8; PAGE_LEN];
                    assert_eq!(
                        swap.load(slot, &mut out),
                        Err(SwapError::Authentication),
                        "tampered record must be rejected"
                    );
                    assert!(out.iter().all(|b| *b == 0), "buffer must be zeroed");
                    tamper_rejected += 1;
                }
                1 => {
                    let other = (slot + 1) % SLOTS;
                    swap.store(slot, &page).expect("re-store");
                    device.relocate(slot, other);
                    let mut out = [0u8; PAGE_LEN];
                    assert_eq!(
                        swap.load(other, &mut out),
                        Err(SwapError::Authentication),
                        "relocated record must be rejected"
                    );
                    tamper_rejected += 1;
                }
                _ => {}
            }
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    assert!(round_trips > 0, "fuzz produced no round-trips");
    assert!(tamper_rejected > 0, "fuzz never exercised the tamper path");
}
