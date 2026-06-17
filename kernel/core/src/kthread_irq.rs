//! [`KthreadIrqWaiter`] — the cooperative [`rustos_kernel_irq::IrqWaiter`]
//! an in-kernel service kthread drives to block on a bound IRQ line
//! (`plans/PI.md` P11 Chunk B-2 INCREMENT (1)).
//!
//! The `irq_wait` syscall handler blocks a *user* task on a bound line
//! through `SyscallIrqWaiter` (in [`crate::syscalls`]), which composes the
//! shared [`rustos_kernel_irq::block_until_ready`] loop with
//! `Scheduler::yield_current` + `KernelArch::monotonic_ns`. An in-kernel
//! service kthread — the INCREMENT (2) root-unlock kthread, which must
//! drive interrupt-driven [`VirtioBlk`]/EMMC2 block I/O before any login
//! can authenticate — has no syscall frame and no `Scheduler` borrow: it
//! suspends cooperatively through the object-safe
//! [`YieldHandle`] the core hands its body.
//!
//! [`KthreadIrqWaiter`] is the second (and only other)
//! [`rustos_kernel_irq::IrqWaiter`] the production kernel installs. It
//! adapts that kthread handle into the same shared blocking loop, so there
//! is **one** IRQ blocking loop in the tree (`AGENTS.md` §2.2): both the
//! syscall path and the kthread path reach
//! [`rustos_kernel_irq::block_until_ready`].
//!
//! Like `SyscallIrqWaiter` it **yields** (re-enqueues, staying runnable)
//! rather than parking: the device-IRQ dispatch path only sets the
//! per-line ready flag through [`rustos_kernel_irq::IrqTable::fire`] and
//! does not re-enqueue a parked task, so a poll/yield cycle is the design
//! that has no lost-wakeup window (the `kernel/irq` crate docs spell this
//! out). Parking would deadlock until that table-internal interlock lands.
//!
//! [`VirtioBlk`]: https://docs.rs/virtio-drivers

use core::cell::RefCell;

use rustos_kernel_irq::{IrqWaitAbort, IrqWaiter};

use crate::kthread::YieldHandle;

/// Cooperative [`IrqWaiter`] for an in-kernel service kthread.
///
/// Holds the kthread's object-safe [`YieldHandle`] and a monotonic-clock
/// closure, both borrowed for the duration of one
/// [`rustos_kernel_irq::block_until_ready`] call. Construct one fresh per
/// blocking wait inside the kthread body, where the
/// [`YieldHandle`] the core supplied is in
/// scope:
///
/// ```ignore
/// let mut handle = YielderHandle::new(yielder);
/// let waiter = KthreadIrqWaiter::new(&mut handle, || arch.monotonic_ns(cpu));
/// match block_until_ready(table, irq_handle, owner, u64::MAX, &waiter) {
///     WaitOutcome::Ready => { /* the device line fired */ }
///     other => { /* timeout / forged / aborted — fail closed */ }
/// }
/// ```
///
/// # Interior mutability
///
/// [`IrqWaiter::yield_now`] takes `&self`, but
/// [`YieldHandle::yield_now`]
/// needs `&mut self` (it suspends the running coroutine). The borrowed
/// handle is therefore wrapped in a [`RefCell`]. This is sound and
/// panic-free: a kthread runs on at most one CPU and
/// [`rustos_kernel_irq::block_until_ready`] calls `now_ns` and `yield_now`
/// strictly serially, never re-entrantly, so the `borrow_mut` is never
/// already held. It is *not* a global mutable static (`AGENTS.md` §2.1) —
/// it lives on the kthread's own stack frame.
pub struct KthreadIrqWaiter<'a, C>
where
    C: Fn() -> u64,
{
    /// The kthread's cooperative-yield handle (the core's
    /// [`YielderHandle`](crate::kthread::YielderHandle) behind the
    /// object-safe trait).
    yielder: RefCell<&'a mut dyn YieldHandle>,
    /// Monotonic clock, in nanoseconds, on the kthread's CPU. Closing over
    /// the arch + CPU keeps the waiter free of any architecture
    /// dependency, mirroring `SyscallIrqWaiter`'s `KernelArch` borrow.
    clock: C,
}

impl<'a, C> KthreadIrqWaiter<'a, C>
where
    C: Fn() -> u64,
{
    /// Adapt `yielder` and the monotonic `clock` into an [`IrqWaiter`].
    #[must_use]
    pub fn new(yielder: &'a mut dyn YieldHandle, clock: C) -> Self {
        Self {
            yielder: RefCell::new(yielder),
            clock,
        }
    }
}

impl<C> IrqWaiter for KthreadIrqWaiter<'_, C>
where
    C: Fn() -> u64,
{
    fn now_ns(&self) -> u64 {
        (self.clock)()
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // A cooperative yield re-enqueues the kthread and resumes here on
        // the next dispatch; the shared loop then re-polls the table. A
        // service kthread cannot "vanish" mid-wait the way a user task
        // can (no `exit` syscall reaps it), so the yield always succeeds —
        // there is no scheduler error to surface (`AGENTS.md` §5.4.5: the
        // failure modes that exist on the syscall path do not apply here).
        self.yielder.borrow_mut().yield_now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::Cell;

    use rustos_abi::IrqHandle;
    use rustos_kernel_irq::{block_until_ready, IrqController, IrqTable, MaskError, WaitOutcome};
    use rustos_kernel_sec::TaskId;

    /// Permissive controller so [`IrqTable::fire`] can set the ready flag
    /// without a real architecture port.
    struct OkController;
    impl IrqController for OkController {
        fn mask(&self, _line: u32) -> Result<(), MaskError> {
            Ok(())
        }
    }

    /// A mock [`YieldHandle`] that counts cooperative yields and runs an
    /// optional hook on the `fire_on`-th yield, modelling "the device
    /// raises its line while the kthread is parked between polls". `park`
    /// is never exercised here — the cooperative waiter only ever calls
    /// `yield_now` (the production design, see the module docs).
    struct MockYielder<'a> {
        yields: &'a Cell<u32>,
        fire_on: Option<u32>,
        hook: Option<&'a dyn Fn()>,
    }

    impl YieldHandle for MockYielder<'_> {
        fn yield_now(&mut self) {
            let n = self.yields.get() + 1;
            self.yields.set(n);
            if let (Some(fire_on), Some(hook)) = (self.fire_on, self.hook) {
                if n == fire_on {
                    hook();
                }
            }
        }

        fn park(&mut self) {
            // The cooperative IRQ waiter never parks; a call here is a bug
            // in the test, not production behaviour.
            panic!("KthreadIrqWaiter must yield, never park");
        }
    }

    #[test]
    fn ready_when_the_line_is_already_fired_consumes_no_yield() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();
        table.fire(7, &OkController).unwrap();

        let yields = Cell::new(0);
        let mut mock = MockYielder {
            yields: &yields,
            fire_on: None,
            hook: None,
        };
        let waiter = KthreadIrqWaiter::new(&mut mock, || 0);

        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Ready
        );
        // A pre-fired binding is consumed on the first poll, before any
        // cooperative yield.
        assert_eq!(yields.get(), 0);
    }

    #[test]
    fn ready_when_the_device_fires_during_a_cooperative_yield() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();

        let yields = Cell::new(0);
        let fire = || {
            table.fire(7, &OkController).unwrap();
        };
        // The device raises its line on the third parked yield — the
        // kthread analogue of `kernel/irq`'s
        // `returns_ready_when_fire_arrives_during_a_yield`.
        let mut mock = MockYielder {
            yields: &yields,
            fire_on: Some(3),
            hook: Some(&fire),
        };
        let waiter = KthreadIrqWaiter::new(&mut mock, || 0);

        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), u64::MAX, &waiter),
            WaitOutcome::Ready
        );
        assert_eq!(yields.get(), 3);
    }

    #[test]
    fn timed_out_when_the_deadline_elapses_without_a_fire() {
        let table = IrqTable::new(31);
        let out = table.bind(7, TaskId(1)).unwrap();

        // A clock that advances 100 ns per reading expires a 250 ns budget
        // after a few polls; the waiter never injects a fire.
        let now = Cell::new(0u64);
        let yields = Cell::new(0);
        let mut mock = MockYielder {
            yields: &yields,
            fire_on: None,
            hook: None,
        };
        let clock = || {
            let v = now.get();
            now.set(v + 100);
            v
        };
        let waiter = KthreadIrqWaiter::new(&mut mock, clock);

        assert_eq!(
            block_until_ready(&table, out.handle, TaskId(1), 250, &waiter),
            WaitOutcome::TimedOut
        );
    }

    #[test]
    fn not_found_on_a_forged_handle_before_any_yield() {
        let table = IrqTable::new(31);
        let yields = Cell::new(0);
        let mut mock = MockYielder {
            yields: &yields,
            fire_on: None,
            hook: None,
        };
        let waiter = KthreadIrqWaiter::new(&mut mock, || 0);

        assert_eq!(
            block_until_ready(
                &table,
                IrqHandle::from_raw(0xDEAD_BEEF),
                TaskId(1),
                u64::MAX,
                &waiter
            ),
            WaitOutcome::NotFound
        );
        assert_eq!(yields.get(), 0);
    }

    #[test]
    fn clock_closure_is_consulted_for_now_ns() {
        let yields = Cell::new(0);
        let mut mock = MockYielder {
            yields: &yields,
            fire_on: None,
            hook: None,
        };
        let waiter = KthreadIrqWaiter::new(&mut mock, || 0xABCD_1234);
        assert_eq!(waiter.now_ns(), 0xABCD_1234);
    }
}
