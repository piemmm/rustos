//! The one definition of an interrupt-driven UART console's receive drain.
//!
//! An interrupt-driven serial console has two destructive readers of the
//! same hardware receive FIFO: the receive interrupt service routine, and
//! the reader's own poll-and-read (a `stream_read` syscall or the boot
//! unlock kthread). Both move bytes out of the FIFO into a `kernel/core`
//! [`ConsoleInputQueue`] whose `push` wakes a parked reader. The subtle
//! part — lossless backpressure so no typed byte is ever dropped, and the
//! clear-then-recheck that closes the lost-wakeup race on the last byte —
//! is **identical** across every UART, so it lives here once and both the
//! aarch64 PL011 and the x86_64 16550 console paths call it (the charter
//! forbids copying this logic into a sibling port).
//!
//! The helper is pure and architecture-neutral: every hardware touch is an
//! injected closure, so it is exercised by host unit tests against a fake
//! FIFO and needs no target to run. The genuinely target-divergent pieces —
//! *how* a byte is read from the FIFO, *how* the receive latch is cleared,
//! and *how* the flow-control brake masks the source — are the closures the
//! caller supplies.

use tairix_kernel_core::{ConsoleDevice, ConsoleInput as _, ConsoleInputQueue};

/// The staging buffer size for one FIFO read pass. A UART receive FIFO is
/// at most a handful of bytes deep (16 on a 16550, 32 on a PL011), so a
/// 32-byte window drains a full FIFO in one or two passes while keeping the
/// helper allocation-free.
const DRAIN_CHUNK: usize = 32;

/// Drain a UART's hardware receive FIFO into the console's type-ahead
/// [`ConsoleInputQueue`], pushing through `device` so the console's
/// cooked-mode line discipline (a `^C`/`^Z` → foreground signal, `plans/
/// SPAWN.md` SP9) sees every byte at arrival time.
///
/// Callers must serialise this against every other FIFO reader (the reader
/// runs with interrupts deliverable, so it genuinely races the receive
/// ISR); an interrupt-masking gate around the whole call is the discipline
/// both ports use.
///
/// The loop is **lossless**: it dequeues from the FIFO only what the queue
/// can accept this instant (`queue.free_capacity()`), leaving any surplus
/// in the hardware FIFO for the next interrupt rather than reading bytes it
/// would have to drop. When the queue is full it invokes `on_full` — the
/// flow-control brake — and stops; the reader re-opens the brake once it has
/// drained queue space, and the level/edge source re-delivers the bytes
/// still in the FIFO. It is **bounded**: it moves at most one queue-capacity
/// of bytes per call.
///
/// * `read_fifo(buf) -> n` reads up to `buf.len()` immediately-available
///   FIFO bytes into `buf`, returning the count (`0` when the FIFO is
///   momentarily empty). It must never block.
/// * `clear_rx()` clears any sticky receive/receive-timeout interrupt latch
///   the device asserts even with a drained FIFO (the PL011 receive-timeout
///   latch); a no-op on hardware without one.
/// * `on_full()` applies the flow-control brake (mask the interrupt source
///   and record it) when the software queue cannot accept more this instant.
pub fn drain_fifo_into_console<R, C, F>(
    device: &ConsoleDevice,
    queue: &ConsoleInputQueue,
    mut read_fifo: R,
    mut clear_rx: C,
    mut on_full: F,
) where
    R: FnMut(&mut [u8]) -> usize,
    C: FnMut(),
    F: FnMut(),
{
    loop {
        let free = queue.free_capacity();
        if free == 0 {
            // The queue is full and the reader has not yet drained it.
            // Reading more would force the surplus to be dropped, which
            // truncates a login line (and its terminating newline) and
            // wedges the line-oriented reader. Apply the flow-control brake
            // and leave the surplus in the FIFO; the reader re-opens the
            // brake once it frees space and the source re-delivers.
            on_full();
            break;
        }
        let mut buf = [0u8; DRAIN_CHUNK];
        let want = free.min(buf.len());
        let n = read_fifo(&mut buf[..want]);
        if n == 0 {
            // The FIFO read empty: clear any sticky receive latch so the
            // line deasserts. A byte that raced in between the empty read
            // and the clear would have had its interrupt cleared with it —
            // stranding it in the FIFO with no re-delivery, the lost-wakeup
            // race the charter forbids. So read once more after the clear: a
            // raced-in byte is drained now, and a byte arriving *after* the
            // clear latches a fresh, uncleared interrupt that re-fires. Only
            // a genuinely still-empty FIFO ends the drain.
            clear_rx();
            let n2 = read_fifo(&mut buf[..want]);
            if n2 == 0 {
                break;
            }
            // `n2 <= want <= free`: the raced-in chunk fits, and its push
            // wakes the parked reader. Loop to keep draining.
            let _ = device.push(&buf[..n2]);
            continue;
        }
        // `n <= want <= free`, so the whole chunk fits and the push wakes
        // the parked reader (`ConsoleInputQueue::push` → `console_wake`).
        let _ = device.push(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::cell::RefCell;
    use tairix_kernel_core::{ConsoleRead as _, ConsoleWrite, CONSOLE_INPUT_QUEUE_CAPACITY};

    /// A write half that discards output — the drain only exercises the
    /// input path.
    struct SinkWrite;
    impl ConsoleWrite for SinkWrite {
        fn write(&self, bytes: &[u8]) -> Result<usize, tairix_abi::Errno> {
            Ok(bytes.len())
        }
    }

    static SINK_WRITE: SinkWrite = SinkWrite;

    /// A fake hardware FIFO: bytes the "device" has received, drained
    /// destructively from the front like a real receive FIFO.
    struct FakeFifo {
        bytes: RefCell<alloc::collections::VecDeque<u8>>,
        clears: RefCell<usize>,
        full_hits: RefCell<usize>,
    }

    impl FakeFifo {
        fn new(initial: &[u8]) -> Self {
            Self {
                bytes: RefCell::new(initial.iter().copied().collect()),
                clears: RefCell::new(0),
                full_hits: RefCell::new(0),
            }
        }
        fn read(&self, buf: &mut [u8]) -> usize {
            let mut q = self.bytes.borrow_mut();
            let mut n = 0;
            for slot in buf.iter_mut() {
                match q.pop_front() {
                    Some(b) => {
                        *slot = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            n
        }
    }

    fn queue_and_device() -> (
        &'static ConsoleInputQueue,
        &'static ConsoleDevice,
    ) {
        // Leak `'static` handles: the host test process is short-lived and a
        // `ConsoleDevice` borrows its halves for `'static`.
        let queue: &'static ConsoleInputQueue = Box::leak(Box::new(ConsoleInputQueue::new()));
        let device: &'static ConsoleDevice = Box::leak(Box::new(ConsoleDevice::with_input(
            &SINK_WRITE,
            queue,
            queue,
        )));
        (queue, device)
    }

    #[test]
    fn drains_a_short_burst_in_order() {
        let (queue, device) = queue_and_device();
        let fifo = FakeFifo::new(b"pass\n");
        drain_fifo_into_console(
            device,
            queue,
            |b| fifo.read(b),
            || *fifo.clears.borrow_mut() += 1,
            || *fifo.full_hits.borrow_mut() += 1,
        );
        let mut out = [0u8; 16];
        let n = queue.read(&mut out).unwrap();
        assert_eq!(&out[..n], b"pass\n");
        // The FIFO was drained empty, so the clear-then-recheck ran once and
        // the flow-control brake never engaged.
        assert_eq!(*fifo.clears.borrow(), 1);
        assert_eq!(*fifo.full_hits.borrow(), 0);
    }

    #[test]
    fn applies_flow_control_and_leaves_surplus_in_the_fifo() {
        let (queue, device) = queue_and_device();
        // One byte more than the queue can hold in a single drain.
        let input: alloc::vec::Vec<u8> = (0..CONSOLE_INPUT_QUEUE_CAPACITY + 8)
            .map(|i| u8::try_from(i % 251).expect("i % 251 < 256"))
            .collect();
        let fifo = FakeFifo::new(&input);
        drain_fifo_into_console(
            device,
            queue,
            |b| fifo.read(b),
            || *fifo.clears.borrow_mut() += 1,
            || *fifo.full_hits.borrow_mut() += 1,
        );
        // The brake engaged exactly once and the surplus stayed in the FIFO
        // (nothing was dropped): the queue is full and the remainder is
        // still readable from the fake device.
        assert_eq!(*fifo.full_hits.borrow(), 1);
        assert_eq!(queue.free_capacity(), 0);
        assert_eq!(fifo.bytes.borrow().len(), 8);
    }

    #[test]
    fn empty_fifo_drains_nothing_and_never_brakes() {
        let (queue, device) = queue_and_device();
        let fifo = FakeFifo::new(b"");
        drain_fifo_into_console(
            device,
            queue,
            |b| fifo.read(b),
            || *fifo.clears.borrow_mut() += 1,
            || *fifo.full_hits.borrow_mut() += 1,
        );
        let mut out = [0u8; 4];
        assert_eq!(queue.read(&mut out).unwrap(), 0);
        // The empty read triggered exactly one clear-then-recheck, no brake.
        assert_eq!(*fifo.clears.borrow(), 1);
        assert_eq!(*fifo.full_hits.borrow(), 0);
    }
}
