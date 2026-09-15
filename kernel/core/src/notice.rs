//! The system-notice registry: one machine-wide value per topic, and the
//! generation a subscriber converges on (`plans/NOTICE.md`).
//!
//! A notice is a **state edge**. Each [`NoticeTopic`] names one value; a
//! subscriber learns only that it moved and then reads the current one. There
//! is therefore no queue here, and nothing to overflow, drop, or hold back —
//! the whole state is `(generation, payload)` per topic, and a waiter's
//! `observed` generation is the whole of its subscription.
//!
//! # Where each topic's truth lives
//!
//! A topic whose value the *kernel* already owns is not stored twice:
//! [`generation`] reads it from its own source and [`payload`] renders it on
//! demand, so the registry cannot disagree with the thing it describes.
//! Only a topic published from user space is retained here.
//!
//! | Topic | Generation | Payload |
//! |---|---|---|
//! | `Desktop` | a counter bumped when the published record differs | retained here |
//! | `Mounts` | a counter bumped by every mount-table mutation | none |
//! | `MemoryPressure` | the published band's depth | rendered from the gauge |
//!
//! The memory-pressure generation is the band depth *itself* rather than a
//! counter, so a band that deepens and relaxes again before a waiter runs
//! correctly reports nothing to do. A counter cannot express that, and the
//! desktop record is too wide to be one, so a desktop value that moves and
//! moves back wakes its subscribers once with nothing changed — which costs
//! them one read and no work, and never misses a real change.
//!
//! # Why a global
//!
//! Like [`crate::waitset`], [`crate::callreg`], and [`crate::waitq`], this is
//! global pure data behind a [`SpinLock`] (never a `static mut`, so not the
//! global mutable static the charter forbids): a topic is published from a
//! syscall, from a mount-table mutation, and from whatever was spending
//! memory when the band moved, and read from every subscriber's wake. None of
//! those owns the others, so a global keyed by the topic is the rendezvous.

use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::notice::{Notice, NoticeTopic, NOTICE_PAYLOAD_MAX};
use tairix_abi::Errno;
use tairix_sync::SpinLock;

/// The retained state of one user-published topic.
#[derive(Copy, Clone)]
struct Slot {
    /// Bumped only when a publish actually changes the payload, so
    /// re-publishing the value already in force wakes nobody.
    generation: u64,
    len: usize,
    bytes: [u8; NOTICE_PAYLOAD_MAX],
}

impl Slot {
    const EMPTY: Self = Self {
        generation: 0,
        len: 0,
        bytes: [0u8; NOTICE_PAYLOAD_MAX],
    };
}

/// The user-published topics' retained payloads, indexed by
/// [`NoticeTopic::as_u32`].
///
/// Sized by the topic set rather than grown: the set is closed, so an array is
/// the whole registry and a lookup is an index.
static SLOTS: SpinLock<[Slot; NoticeTopic::ALL.len()]> =
    SpinLock::new([Slot::EMPTY; NoticeTopic::ALL.len()]);

/// The mount table's change generation: bumped by every mutation of the
/// table, which is the whole of the `Mounts` topic's news.
///
/// An atomic rather than a slot in [`SLOTS`]: a mount mutation runs with the
/// filesystem's own locks held, so it must not take another.
static MOUNT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Record that the mount table's composition changed, and flag a wake of
/// every `Mounts` subscriber.
///
/// Called from each [`crate::fs::MountTable`] mutator, so no call site that
/// attaches, re-backs, or removes a mount can forget to.
pub fn mounts_changed() {
    MOUNT_GENERATION.fetch_add(1, Ordering::Release);
    crate::waitq::notice_wake();
}

/// The generation of `topic`: the value a subscriber's `observed` is compared
/// against, and the one definition the readiness scan uses for every topic.
#[must_use]
pub fn generation(topic: NoticeTopic) -> u64 {
    match topic {
        NoticeTopic::Desktop => SLOTS.lock()[topic.as_u32() as usize].generation,
        NoticeTopic::Mounts => MOUNT_GENERATION.load(Ordering::Acquire),
        // The band depth *is* the generation, so a move and a move back
        // leave a waiter's view already correct.
        NoticeTopic::MemoryPressure => {
            u64::from(crate::memstats::MEM_STATS.published_band().depth())
        }
    }
}

/// Write `topic`'s current payload into `out`, answering its length.
///
/// A topic no publisher has reached yet answers `None`: there is no value to
/// converge on, and fabricating one would have a subscriber adopt a desktop
/// the session never described (fail closed).
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if `out` cannot hold the topic's payload.
pub fn payload(topic: NoticeTopic, out: &mut [u8]) -> Result<Option<usize>, Errno> {
    match topic {
        NoticeTopic::Desktop => {
            let slot = SLOTS.lock()[topic.as_u32() as usize];
            if slot.len == 0 {
                return Ok(None);
            }
            let Some(dst) = out.get_mut(..slot.len) else {
                return Err(Errno::LengthOutOfRange);
            };
            dst.copy_from_slice(&slot.bytes[..slot.len]);
            Ok(Some(slot.len))
        }
        NoticeTopic::Mounts => Notice::Mounts.encode(out).map(Some),
        NoticeTopic::MemoryPressure => Notice::MemoryPressure {
            band: crate::memstats::MEM_STATS.published_band().depth(),
        }
        .encode(out)
        .map(Some),
    }
}

/// Publish `notice` as `topic`'s current value, answering whether it moved.
///
/// A publish of the value already in force answers `false` and bumps no
/// generation, so a publisher may re-state the current value freely without
/// waking a single subscriber. A real change flags the wake; the unpark itself
/// runs at the next dispatcher-context drain, exactly as every other deferred
/// wake does.
///
/// # Errors
///
/// [`Errno::PermissionDenied`] for a topic the kernel owns — its value is not
/// user space's to assert. [`Errno::LengthOutOfRange`] is unreachable for a
/// well-formed [`Notice`] (its payload is bounded by the ceiling the slot is
/// sized to) and is reported rather than ignored.
pub fn publish(notice: &Notice) -> Result<bool, Errno> {
    let topic = notice.topic();
    if !matches!(topic, NoticeTopic::Desktop) {
        return Err(Errno::PermissionDenied);
    }
    let mut bytes = [0u8; NOTICE_PAYLOAD_MAX];
    let len = notice.encode(&mut bytes)?;
    let moved = {
        let mut slots = SLOTS.lock();
        let slot = &mut slots[topic.as_u32() as usize];
        if slot.len == len && slot.bytes[..len] == bytes[..len] {
            false
        } else {
            slot.len = len;
            slot.bytes = bytes;
            slot.generation = slot.generation.wrapping_add(1);
            true
        }
    };
    if moved {
        crate::waitq::notice_wake();
    }
    Ok(moved)
}

/// Serialise the tests that *steer* a user-published topic, and clear the
/// registry for the one now holding the guard.
///
/// The registry is process-global while the test binary runs tests
/// concurrently, so two tests publishing the desktop at once would each see
/// the other's value and neither would be testing anything. Distinct payloads
/// do not prevent it; only one test being in the registry at a time does.
#[cfg(test)]
pub(crate) fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *SLOTS.lock() = [Slot::EMPTY; NoticeTopic::ALL.len()];
    guard
}

#[cfg(test)]
mod tests {
    use super::{generation, mounts_changed, payload, publish, registry_guard};
    use tairix_abi::desktop::{Appearance, DesktopInfo};
    use tairix_abi::notice::{Notice, NoticeTopic, NOTICE_PAYLOAD_MAX};
    use tairix_abi::Errno;

    fn desktop(scale: u16, appearance: Appearance) -> Notice {
        match DesktopInfo::new(1024, 768, scale, appearance) {
            Ok(info) => Notice::Desktop(info),
            Err(_) => unreachable!("a valid desktop"),
        }
    }

    #[test]
    fn an_unpublished_topic_answers_nothing_rather_than_a_guess() {
        let _registry = registry_guard();
        let mut out = [0u8; NOTICE_PAYLOAD_MAX];
        assert_eq!(payload(NoticeTopic::Desktop, &mut out), Ok(None));
    }

    #[test]
    fn a_publish_moves_the_generation_and_is_readable_back() {
        let _registry = registry_guard();
        let before = generation(NoticeTopic::Desktop);
        let notice = desktop(100, Appearance::Dark);
        assert_eq!(publish(&notice), Ok(true));
        assert_ne!(generation(NoticeTopic::Desktop), before);
        let mut out = [0u8; NOTICE_PAYLOAD_MAX];
        let len = payload(NoticeTopic::Desktop, &mut out)
            .expect("a published topic")
            .expect("a payload");
        assert_eq!(
            Notice::decode(NoticeTopic::Desktop, &out[..len]),
            Ok(notice)
        );
    }

    /// Re-stating the value already in force is not news: a publisher may do
    /// it freely without waking a single subscriber.
    #[test]
    fn republishing_the_same_value_moves_nothing() {
        let _registry = registry_guard();
        let notice = desktop(150, Appearance::Light);
        assert_eq!(publish(&notice), Ok(true));
        let after_first = generation(NoticeTopic::Desktop);
        assert_eq!(publish(&notice), Ok(false));
        assert_eq!(generation(NoticeTopic::Desktop), after_first);
    }

    #[test]
    fn each_distinct_value_is_one_edge() {
        let _registry = registry_guard();
        assert_eq!(publish(&desktop(100, Appearance::Dark)), Ok(true));
        let dark = generation(NoticeTopic::Desktop);
        assert_eq!(publish(&desktop(100, Appearance::Light)), Ok(true));
        let light = generation(NoticeTopic::Desktop);
        assert_ne!(dark, light);
        assert_eq!(publish(&desktop(100, Appearance::Light)), Ok(false));
        assert_eq!(generation(NoticeTopic::Desktop), light);
    }

    #[test]
    fn a_kernel_owned_topic_refuses_a_publish() {
        let _registry = registry_guard();
        assert_eq!(publish(&Notice::Mounts), Err(Errno::PermissionDenied));
        assert_eq!(
            publish(&Notice::MemoryPressure { band: 0 }),
            Err(Errno::PermissionDenied)
        );
    }

    /// Monotone in the shared counter (observe, bump, observe greater)
    /// rather than an exact step: the counter is process-global and the
    /// test binary runs tests concurrently, so pinning the increment would
    /// be a race, not a test.
    #[test]
    fn a_mount_mutation_moves_the_mounts_generation() {
        let before = generation(NoticeTopic::Mounts);
        mounts_changed();
        assert!(generation(NoticeTopic::Mounts) > before);
    }

    /// The payload-less topic still reads back — as nothing — so a subscriber
    /// that reads every topic the same way is not special-cased.
    #[test]
    fn the_mounts_topic_reads_back_an_empty_payload() {
        let mut out = [0xAAu8; NOTICE_PAYLOAD_MAX];
        assert_eq!(payload(NoticeTopic::Mounts, &mut out), Ok(Some(0)));
        assert_eq!(out, [0xAA; NOTICE_PAYLOAD_MAX]);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_truncating_a_payload() {
        let _registry = registry_guard();
        assert_eq!(publish(&desktop(100, Appearance::Dark)), Ok(true));
        let mut tiny = [0u8; DesktopInfo::WIRE_LEN - 1];
        assert_eq!(
            payload(NoticeTopic::Desktop, &mut tiny),
            Err(Errno::LengthOutOfRange)
        );
    }
}
