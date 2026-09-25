//! [`MockHost`], the in-process [`VirtioHost`] every virtio driver's unit tests
//! run on. Built only for tests, behind the crate's `mock` feature.

use super::{CompletionSignal, DmaHost, VirtioHost};
use crate::dma::{DmaSlab, PoolId};
use crate::transport::MockTransport;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cell::RefCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, Ordering};
use tairix_abi::DriverError;

/// In-process [`VirtioHost`] the unit tests of this crate and of every virtio
/// driver run against.
///
/// DMA comes from leaked boxes: the `phys` of a slab is its CPU pointer cast to
/// `u64`, the identity mapping of the test process, and a slab's drop only
/// records the release, and whether the slab came back zeroed (see
/// [`Self::slabs_outstanding`] and [`Self::released_zeroed`]).
///
/// A wait plays the next [`MockWait`] a test [scripted](Self::script_waits),
/// else the host's standing one ([`MockWait::Answer`] unless built
/// [silent](Self::silent)). An [attached](Self::attach) device answers a wait
/// by draining the waited queue, so a driver's completion path runs as it does
/// against a device completing on its interrupt. The host keeps the clock its
/// waits spend.
pub struct MockHost {
    notify_log: RefCell<Vec<u16>>,
    bytes_allocated: Cell<usize>,
    quiesced: Cell<usize>,
    /// What became of each slab minted so far, indexed by slot.
    slabs: RefCell<Vec<Arc<AtomicU8>>>,
    clock_ns: Cell<u64>,
    clock_reads: Cell<usize>,
    script: RefCell<VecDeque<MockWait>>,
    standing: MockWait,
    device: RefCell<Option<Rc<RefCell<MockTransport>>>>,
}

/// How one [`MockHost`] wait plays out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MockWait {
    /// The attached device, if any, completes what the waited queue holds,
    /// and the wait fires at once.
    Answer,
    /// The wait fires `after_ns` in, with nothing done: an early wake, or one
    /// for another queue on a shared line. One past the budget is silence.
    Spurious {
        /// How far into the wait the wake lands.
        after_ns: u64,
    },
    /// Nothing happens for the whole budget.
    Silent,
    /// The wait could not be made at all — a revoked or refused interrupt
    /// binding — so it times out at once, with no time spent.
    Refused,
    /// The attached device completes what the waited queue holds, but the
    /// interrupt is lost: the wait runs out its whole budget.
    Lost,
}

impl Default for MockHost {
    fn default() -> Self {
        Self::with_standing(MockWait::Answer)
    }
}

/// A slab still held, or withheld and so never released.
const HELD: u8 = 0;
/// A slab released with every byte zero.
const RELEASED_ZEROED: u8 = 1;
/// A slab released still holding data.
const RELEASED_DIRTY: u8 = 2;

/// Records one mock slab's release and whether it came back zeroed. Each slab
/// owns one strong count of its record, so the record stays valid however
/// long the slab outlives its host.
///
/// # Safety
///
/// `fate` is the pointer [`Arc::into_raw`] minted for this slab alone, and
/// `cpu`/`len` are the storage the slab was minted over.
unsafe fn record_mock_release(fate: *const (), cpu: NonNull<u8>, _slot: usize, len: usize) {
    // SAFETY: the caller passes the slab's own `into_raw` pointer, and a
    // slab's drop runs once, so this consumes the count exactly once.
    let fate = unsafe { Arc::from_raw(fate.cast::<AtomicU8>()) };
    // SAFETY: the storage is a leaked, initialised `len`-byte allocation, and
    // the slab handing it back holds no borrow of it any more.
    let bytes = unsafe { core::slice::from_raw_parts(cpu.as_ptr(), len) };
    let outcome = if bytes.iter().all(|b| *b == 0) {
        RELEASED_ZEROED
    } else {
        RELEASED_DIRTY
    };
    fate.store(outcome, Ordering::Relaxed);
}

impl MockHost {
    /// Construct an empty mock host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A host none of whose unscripted waits is ever answered: a device whose
    /// interrupt is lost for good.
    #[must_use]
    pub fn silent() -> Self {
        Self::with_standing(MockWait::Silent)
    }

    fn with_standing(standing: MockWait) -> Self {
        Self {
            notify_log: RefCell::new(Vec::new()),
            bytes_allocated: Cell::new(0),
            quiesced: Cell::new(0),
            slabs: RefCell::new(Vec::new()),
            clock_ns: Cell::new(0),
            clock_reads: Cell::new(0),
            script: RefCell::new(VecDeque::new()),
            standing,
            device: RefCell::new(None),
        }
    }

    /// Answer waits by draining `device`, the mock the driver under test
    /// drives through its own handle.
    pub fn attach(&self, device: &Rc<RefCell<MockTransport>>) {
        *self.device.borrow_mut() = Some(Rc::clone(device));
    }

    /// Play `waits`, in order, for the next waits.
    pub fn script_waits(&self, waits: impl IntoIterator<Item = MockWait>) {
        self.script.borrow_mut().extend(waits);
    }

    /// Have the attached device, if any, complete what `queue_index` holds.
    fn play_device(&self, queue_index: u16) {
        if let Some(device) = self.device.borrow().as_ref() {
            // A queue index the device lacks is the driver's own error, which
            // its next ring read reports.
            let _ = device.borrow_mut().drain_queue(queue_index);
        }
    }

    /// How many times a driver read the clock: each reading costs a real host
    /// a system call.
    #[must_use]
    pub fn clock_reads(&self) -> usize {
        self.clock_reads.get()
    }

    /// All notify events the host has seen so far, in order.
    #[must_use]
    pub fn notify_log(&self) -> Vec<u16> {
        self.notify_log.borrow().clone()
    }

    /// Total number of bytes ever handed out by this host.
    ///
    /// Because the test mock leaks its backing allocations for the
    /// lifetime of the unit-test process (see
    /// [`Self::alloc_dma_zeroed`]) this counter is monotonic.
    #[must_use]
    pub fn bytes_allocated(&self) -> usize {
        self.bytes_allocated.get()
    }

    /// How many times a driver declared its device quiesced.
    #[must_use]
    pub fn quiesced_calls(&self) -> usize {
        self.quiesced.get()
    }

    /// Slabs this host minted that have not been released: what a driver
    /// still holds, or deliberately withheld from a device it could not stop.
    #[must_use]
    pub fn slabs_outstanding(&self) -> usize {
        self.slabs
            .borrow()
            .iter()
            .filter(|fate| fate.load(Ordering::Relaxed) == HELD)
            .count()
    }

    /// Whether the slab minted as `slot` has been released with every byte
    /// zero, as memory that carried a secret must be.
    #[must_use]
    pub fn released_zeroed(&self, slot: usize) -> bool {
        self.slabs
            .borrow()
            .get(slot)
            .is_some_and(|fate| fate.load(Ordering::Relaxed) == RELEASED_ZEROED)
    }
}

impl DmaHost for MockHost {
    /// Hand out a zeroed [`DmaSlab`] backed by a leaked `Box<[u8]>`, so the
    /// slab carries its pointer with no borrow; the 64 MiB cap bounds what a
    /// test can leak.
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        if size == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        // 64 MiB pool cap is far above the Stage-4 unit-test budget;
        // exceeding it signals a runaway test rather than real
        // allocator pressure. Failing closed.
        let bytes_now = self.bytes_allocated.get();
        let Some(bytes_after) = bytes_now.checked_add(size) else {
            return Err(DriverError::LengthOutOfRange);
        };
        if bytes_after > 64 * 1024 * 1024 {
            return Err(DriverError::LengthOutOfRange);
        }
        let storage: Box<[u8]> = alloc::vec![0u8; size].into_boxed_slice();
        let ptr = NonNull::from(Box::leak(storage)).cast::<u8>();
        // Exposed after the leak, which invalidates any pointer taken before
        // it: the device reaches the bytes by this address.
        let phys = ptr.as_ptr().expose_provenance() as u64;
        let fate = Arc::new(AtomicU8::new(HELD));
        let slot = self.slabs.borrow().len();
        self.slabs.borrow_mut().push(Arc::clone(&fate));
        self.bytes_allocated.set(bytes_after);
        let fate = Arc::into_raw(fate).cast::<()>();
        // SAFETY: `ptr` is the only handle on a leaked, `'static` allocation
        // of exactly `size` bytes, so the slab owns it alone. `fate` is the
        // slab's own strong count, which `record_mock_release` consumes.
        Ok(unsafe {
            DmaSlab::from_pool(
                phys,
                ptr,
                size,
                PoolId::MOCK,
                slot,
                fate,
                record_mock_release,
            )
        })
    }

    fn device_quiesced(&self) {
        self.quiesced.set(self.quiesced.get() + 1);
    }
}

impl VirtioHost for MockHost {
    /// Records the wait and plays the next [`MockWait`]; the mock never blocks,
    /// it only moves its clock by the time the wait would have taken.
    fn notify_wait(&self, queue_index: u16, timeout_ns: u64) -> CompletionSignal {
        self.notify_log.borrow_mut().push(queue_index);
        let wait = self
            .script
            .borrow_mut()
            .pop_front()
            .unwrap_or(self.standing);
        let (elapsed_ns, signal) = match wait {
            MockWait::Answer => {
                self.play_device(queue_index);
                (0, CompletionSignal::Fired)
            }
            MockWait::Spurious { after_ns } if after_ns < timeout_ns => {
                (after_ns, CompletionSignal::Fired)
            }
            MockWait::Spurious { .. } | MockWait::Silent => {
                (timeout_ns, CompletionSignal::TimedOut)
            }
            MockWait::Refused => (0, CompletionSignal::TimedOut),
            MockWait::Lost => {
                self.play_device(queue_index);
                (timeout_ns, CompletionSignal::TimedOut)
            }
        };
        self.clock_ns
            .set(self.clock_ns.get().saturating_add(elapsed_ns));
        signal
    }

    fn now_ns(&self) -> u64 {
        self.clock_reads.set(self.clock_reads.get() + 1);
        self.clock_ns.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::driver::BufferClass;

    #[test]
    fn mock_host_zero_initialises() {
        let host = MockHost::new();
        let slab = host.alloc_dma_zeroed(64).expect("alloc");
        assert_eq!(slab.len(), 64);
        assert!(slab.as_bytes().iter().all(|b| *b == 0));
        assert_eq!(slab.pool_id(), PoolId::MOCK);
    }

    #[test]
    fn mock_host_rejects_zero_size() {
        let host = MockHost::new();
        assert!(matches!(
            host.alloc_dma_zeroed(0),
            Err(DriverError::BufferTooSmall)
        ));
    }

    #[test]
    fn mock_host_records_notifies() {
        let host = MockHost::new();
        assert_eq!(host.notify_wait(0, u64::MAX), CompletionSignal::Fired);
        assert_eq!(host.notify_wait(1, u64::MAX), CompletionSignal::Fired);
        assert_eq!(host.notify_wait(0, u64::MAX), CompletionSignal::Fired);
        assert_eq!(host.notify_log(), alloc::vec![0u16, 1, 0]);
    }

    #[test]
    fn a_silent_host_spends_each_waits_whole_budget() {
        let host = MockHost::silent();
        assert_eq!(host.notify_wait(0, 7), CompletionSignal::TimedOut);
        assert_eq!(host.notify_wait(0, 5), CompletionSignal::TimedOut);
        assert_eq!(host.now_ns(), 12);
    }

    #[test]
    fn scripted_waits_play_in_order_then_the_standing_one() {
        let host = MockHost::new();
        host.script_waits([
            MockWait::Silent,
            MockWait::Spurious { after_ns: 3 },
            MockWait::Spurious { after_ns: 50 },
        ]);
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::TimedOut);
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::Fired);
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::TimedOut);
        assert_eq!(host.now_ns(), 23, "10, then 3, then a whole 10");
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::Fired);
        assert_eq!(host.now_ns(), 23, "an answer takes no time");
    }

    #[test]
    fn a_refused_wait_times_out_at_once_and_a_lost_one_spends_its_budget() {
        let host = MockHost::new();
        host.script_waits([MockWait::Refused, MockWait::Lost]);
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::TimedOut);
        assert_eq!(host.now_ns(), 0);
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::TimedOut);
        assert_eq!(host.now_ns(), 10);
    }

    #[test]
    fn an_attached_device_answers_the_waited_queue() {
        use crate::queue::{ChainSegment, SplitQueue};
        use crate::transport::{Direction, MockTransport};

        let host = MockHost::new();
        let device = MockTransport::new(2, 4, 0, 0).into_shared();
        let mut transport = alloc::rc::Rc::clone(&device);
        host.attach(&device);
        let mut q = SplitQueue::new(&mut transport, &host, 1, 4, 1).unwrap();
        device.borrow_mut().install_shim(
            1,
            Box::new(|_chain: &mut crate::transport::ChainView<'_>| Ok(0)),
        );
        let slab = host.alloc_dma_zeroed(4).unwrap();
        q.add_chain(&[ChainSegment {
            phys: slab.phys(),
            len: 4,
            direction: Direction::DeviceWrite,
        }])
        .unwrap();
        assert_eq!(host.notify_wait(0, 10), CompletionSignal::Fired);
        assert!(
            q.poll_used().is_err(),
            "another queue's wait drains nothing"
        );
        assert_eq!(host.notify_wait(1, 10), CompletionSignal::Fired);
        assert!(q.poll_used().is_ok());
    }

    #[test]
    fn mock_host_assigns_distinct_slots() {
        let host = MockHost::new();
        let a = host.alloc_dma_zeroed(4).unwrap();
        let b = host.alloc_dma_zeroed(4).unwrap();
        let c = host.alloc_dma_zeroed(4).unwrap();
        assert_ne!(a.slot(), b.slot());
        assert_ne!(b.slot(), c.slot());
        assert_ne!(a.slot(), c.slot());
    }

    #[test]
    fn a_mock_slab_counts_its_release_once_even_past_its_host() {
        let host = MockHost::new();
        let kept = host.alloc_dma_zeroed(8).unwrap();
        let dropped = host.alloc_dma_zeroed(8).unwrap();
        let withheld = host.alloc_dma_zeroed(8).unwrap();
        assert_eq!(host.slabs_outstanding(), 3);
        drop(dropped);
        core::mem::forget(withheld);
        assert_eq!(host.slabs_outstanding(), 2);
        drop(host);
        // Each slab owns a count of its own record, so a release after its
        // host is gone is still sound.
        drop(kept);
    }

    #[test]
    fn a_released_slab_reports_whether_it_came_back_zeroed() {
        let host = MockHost::new();
        let mut dirty = host.alloc_dma_zeroed(8).unwrap();
        let mut scrubbed = host.alloc_dma_zeroed(8).unwrap();
        let mut withheld = host.alloc_dma_zeroed(8).unwrap();
        for slab in [&mut dirty, &mut scrubbed, &mut withheld] {
            slab.as_bytes_mut()[7] = 0x5A;
        }
        let (dirty_slot, scrubbed_slot, withheld_slot) =
            (dirty.slot(), scrubbed.slot(), withheld.slot());
        assert!(!host.released_zeroed(scrubbed_slot), "still held");
        crate::dma::scrub(&mut scrubbed);
        withheld.withhold();
        drop((dirty, scrubbed, withheld));
        assert!(!host.released_zeroed(dirty_slot));
        assert!(host.released_zeroed(scrubbed_slot));
        assert!(!host.released_zeroed(withheld_slot), "never released");
        assert!(!host.released_zeroed(withheld_slot + 1), "never minted");
    }

    #[test]
    fn host_dma_slab_supports_bounce_buffer_round_trip() {
        let host = MockHost::new();
        let slab = host.alloc_dma_zeroed(32).expect("alloc");
        let mut bb = crate::dma::BounceBuffer::new(slab, BufferClass::NonSensitive);
        bb.stage(&[0x42; 16]).unwrap();
        assert_eq!(bb.staged(), &[0x42; 16]);
    }
}
