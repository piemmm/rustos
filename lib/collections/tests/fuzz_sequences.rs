//! Deterministic fuzz harness for the sequence containers.
//!
//! The lengths and text these see are attacker-influenced by design: a boot
//! audit line carries caller-controlled text into an `ArrayString`, a console's
//! type-ahead ring takes whatever a keyboard produces, and the early-boot log
//! ring frames bodies of any length up to its cap. So this drives each
//! container against a naive model over a random operation stream, asserting:
//!
//! 1. Each container agrees with its model after every single operation.
//! 2. Nothing panics, whatever the operation order, length, or text.
//! 3. An `ArrayString` never stores a partial character, however the text was
//!    cut, so reading it back as UTF-8 can never fail.
//! 4. Every element is dropped exactly once — a bulk push, a wrapped drain, an
//!    eviction, and a spill included.
//! 5. A `SecretRing` leaves nothing but the blank in a slot it has vacated.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use tairix_collections::{ArrayString, ArrayVec, RingBuf, SecretRing, SmallVec};
use tairix_fuzzseed::Lcg;

/// Rounds run per sweep. Miri interprets every operation, so it drives a
/// handful: it is hunting undefined behaviour in the containers' unsafe cores,
/// which one round already exercises, and the wide input search is the ordinary
/// and budgeted runs'.
const ROUNDS_PER_SWEEP: u32 = if cfg!(miri) { 2 } else { 150 };

/// Operations applied per round, per container.
const OPS_PER_ROUND: u32 = if cfg!(miri) { 60 } else { 300 };

/// Inline bound the vectors under test are built at.
const INLINE: usize = 8;

/// Byte capacity the rings under test are built at. Deliberately not a power of
/// two, so a bulk push lands at every offset relative to the wrap.
const RING: usize = 13;

/// A value that reports its own destruction, so a leaked or doubly-dropped
/// element is a failure rather than a silence.
struct Tracked {
    tag: u64,
    live: Rc<Cell<i64>>,
}

impl Tracked {
    fn new(tag: u64, live: &Rc<Cell<i64>>) -> Self {
        live.set(live.get() + 1);
        Self {
            tag,
            live: Rc::clone(live),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}

/// Drive an `ArrayVec` of tracked elements against a `Vec` model.
fn sweep_arrayvec(prng: &mut Lcg, live: &Rc<Cell<i64>>) {
    let mut vec: ArrayVec<Tracked, INLINE> = ArrayVec::new();
    let mut model: Vec<u64> = Vec::new();
    for tag in 0..u64::from(OPS_PER_ROUND) {
        match prng.next_u64() % 6 {
            0 | 1 => {
                let pushed = vec.try_push(Tracked::new(tag, live)).is_ok();
                assert_eq!(pushed, model.len() < INLINE);
                if pushed {
                    model.push(tag);
                }
            }
            2 => assert_eq!(vec.pop().map(|t| t.tag), model.pop()),
            3 => {
                let index = usize::try_from(prng.next_u64() % 12).expect("small");
                let taken = vec.remove(index).map(|t| t.tag);
                let expect = (index < model.len()).then(|| model.remove(index));
                assert_eq!(taken, expect);
            }
            4 => {
                let index = usize::try_from(prng.next_u64() % 12).expect("small");
                let taken = vec.swap_remove(index).map(|t| t.tag);
                let expect = (index < model.len()).then(|| model.swap_remove(index));
                assert_eq!(taken, expect);
            }
            _ => {
                let keep = prng.next_u64();
                vec.retain(|t| t.tag % 3 != keep % 3);
                model.retain(|t| t % 3 != keep % 3);
            }
        }
        assert_eq!(vec.len(), model.len(), "length diverged");
        assert!(
            vec.iter().map(|t| t.tag).eq(model.iter().copied()),
            "contents diverged"
        );
    }
}

/// Drive a `SmallVec` across its spill against a `Vec` model.
fn sweep_smallvec(prng: &mut Lcg, live: &Rc<Cell<i64>>) {
    let mut vec: SmallVec<Tracked, INLINE> = SmallVec::new();
    let mut model: Vec<u64> = Vec::new();
    let mut spilled = false;
    for tag in 0..u64::from(OPS_PER_ROUND) {
        match prng.next_u64() % 5 {
            0..=2 => {
                vec.try_push(Tracked::new(tag, live))
                    .expect("the host heap");
                model.push(tag);
            }
            3 => assert_eq!(vec.pop().map(|t| t.tag), model.pop()),
            _ => {
                let index = usize::try_from(prng.next_u64() % 20).expect("small");
                let taken = vec.remove(index).map(|t| t.tag);
                let expect = (index < model.len()).then(|| model.remove(index));
                assert_eq!(taken, expect);
            }
        }
        assert_eq!(vec.len(), model.len(), "length diverged");
        assert!(
            vec.iter().map(|t| t.tag).eq(model.iter().copied()),
            "contents diverged"
        );
        assert_eq!(vec.spilled(), model.len() > INLINE || spilled);
        // Once spilled the vector stays spilled, whatever it shrinks back to.
        spilled |= vec.spilled();
    }
}

/// Push arbitrary text at an `ArrayString` under both policies. Whatever the
/// bytes, the stored prefix must be valid UTF-8 and never a partial character.
fn sweep_arraystring(prng: &mut Lcg) {
    const CAP: usize = 11;
    let mut text = String::new();
    for _ in 0..OPS_PER_ROUND {
        // A mix of ASCII and multi-byte characters, so a cut lands inside a
        // character as often as not.
        let pool = ['a', 'z', 'é', '☃', '𝄞', ' '];
        text.clear();
        let chars = prng.next_u64() % 20;
        for _ in 0..chars {
            let index = usize::try_from(prng.next_u64() % pool.len() as u64).expect("small");
            text.push(pool[index]);
        }

        let truncating = ArrayString::<CAP>::from_str_truncating(&text);
        assert!(truncating.len() <= CAP);
        assert!(text.starts_with(truncating.as_str()), "not a prefix");
        assert_eq!(
            truncating.as_bytes(),
            truncating.as_str().as_bytes(),
            "the stored bytes must be exactly the stored text"
        );
        // The longest prefix that fits: adding the next character would not.
        if let Some(next) = text[truncating.len()..].chars().next() {
            assert!(truncating.len() + next.len_utf8() > CAP, "cut too early");
        }

        let mut refusing: ArrayString<CAP> = ArrayString::new();
        assert_eq!(refusing.try_push_str(&text).is_ok(), text.len() <= CAP);
        assert_eq!(
            refusing.as_str(),
            if text.len() <= CAP { text.as_str() } else { "" },
            "a refused push must store nothing"
        );

        // A truncation to any byte index lands on a character boundary.
        let mut cut = truncating;
        cut.truncate(usize::try_from(prng.next_u64() % (CAP as u64 + 2)).expect("small"));
        assert!(truncating.as_str().starts_with(cut.as_str()));
        assert_eq!(cut.as_bytes(), cut.as_str().as_bytes());
    }
}

/// Drive a `RingBuf` of tracked elements at both ends against a `VecDeque`.
fn sweep_ringbuf(prng: &mut Lcg, live: &Rc<Cell<i64>>) {
    let mut ring: RingBuf<Tracked, INLINE> = RingBuf::new();
    let mut model: VecDeque<u64> = VecDeque::new();
    for tag in 0..u64::from(OPS_PER_ROUND) {
        match prng.next_u64() % 7 {
            0 => {
                let pushed = ring.try_push_back(Tracked::new(tag, live)).is_ok();
                assert_eq!(pushed, model.len() < INLINE);
                if pushed {
                    model.push_back(tag);
                }
            }
            1 => {
                let pushed = ring.try_push_front(Tracked::new(tag, live)).is_ok();
                assert_eq!(pushed, model.len() < INLINE);
                if pushed {
                    model.push_front(tag);
                }
            }
            2 => {
                let evicted = ring.push_back_overwrite(Tracked::new(tag, live));
                if model.len() == INLINE {
                    assert_eq!(evicted.map(|t| t.tag), model.pop_front());
                } else {
                    assert!(evicted.is_none());
                }
                model.push_back(tag);
            }
            3 => assert_eq!(ring.pop_front().map(|t| t.tag), model.pop_front()),
            4 => assert_eq!(ring.pop_back().map(|t| t.tag), model.pop_back()),
            5 => {
                let count = usize::try_from(prng.next_u64() % 12).expect("small");
                let taken = ring.discard_front(count);
                let expect = count.min(model.len());
                assert_eq!(taken, expect);
                model.drain(..expect);
            }
            _ => {
                let offset = usize::try_from(prng.next_u64() % 12).expect("small");
                assert_eq!(ring.get(offset).map(|t| t.tag), model.get(offset).copied());
            }
        }
        assert_eq!(ring.len(), model.len(), "length diverged");
        assert!(
            ring.iter().map(|t| t.tag).eq(model.iter().copied()),
            "contents diverged"
        );
    }
}

/// Drive a `SecretRing` of bytes through bulk pushes and drains. Beyond
/// matching the model, no slot it has vacated may hold anything but the blank.
fn sweep_secret_ring(prng: &mut Lcg) {
    // A non-zero blank, so a slot that was never scrubbed reads differently
    // from one that was zeroed by chance.
    const BLANK: u8 = 0xa5;
    let mut ring: SecretRing<u8, RING> = SecretRing::new(BLANK);
    let mut model: VecDeque<u8> = VecDeque::new();
    let mut scratch = [0u8; RING + 4];
    for _ in 0..OPS_PER_ROUND {
        match prng.next_u64() % 4 {
            0 | 1 => {
                let len = usize::try_from(prng.next_u64() % (RING as u64 + 4)).expect("small");
                let mut bytes = vec![0u8; len];
                prng.fill(&mut bytes);
                // The blank must not appear in the payload, or a stale byte
                // could masquerade as a scrubbed slot.
                for byte in &mut bytes {
                    if *byte == BLANK {
                        *byte = 0;
                    }
                }
                let taken = ring.push_slice(&bytes);
                assert_eq!(taken, bytes.len().min(RING - model.len()));
                model.extend(&bytes[..taken]);
            }
            2 => assert_eq!(ring.pop_front(), model.pop_front()),
            _ => {
                let count = usize::try_from(prng.next_u64() % (RING as u64 + 2)).expect("small");
                let taken = ring.discard_front(count);
                assert_eq!(taken, count.min(model.len()));
                model.drain(..taken);
            }
        }
        assert_eq!(ring.len(), model.len(), "length diverged");
        assert!(ring.iter().copied().eq(model.iter().copied()), "diverged");

        // Every byte of the store is either a queued element or the blank: no
        // slot retains a value the ring has handed on.
        let queued = ring.peek_slice(0, &mut scratch);
        assert_eq!(queued, model.len());
        let store = ring.backing_store();
        assert_eq!(store.len(), RING);
        let live: usize = store.iter().filter(|&&byte| byte != BLANK).count();
        assert!(
            live <= model.len(),
            "the store holds {live} non-blank bytes but only {} are queued",
            model.len()
        );
    }
    ring.purge();
    assert!(ring.backing_store().iter().all(|&byte| byte == BLANK));
}

#[test]
fn sequence_containers_match_their_models_and_never_panic() {
    let mut prng = Lcg::new(tairix_fuzzseed::start(
        "sequence_containers_match_their_models_and_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..ROUNDS_PER_SWEEP {
            let live = Rc::new(Cell::new(0i64));
            sweep_arrayvec(&mut prng, &live);
            assert_eq!(live.get(), 0, "an `ArrayVec` leaked or double-dropped");
            sweep_smallvec(&mut prng, &live);
            assert_eq!(live.get(), 0, "a `SmallVec` leaked or double-dropped");
            sweep_ringbuf(&mut prng, &live);
            assert_eq!(live.get(), 0, "a `RingBuf` leaked or double-dropped");
            sweep_arraystring(&mut prng);
            sweep_secret_ring(&mut prng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
