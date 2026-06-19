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
/// let coop = CooperativeYield::new(&mut handle);
/// let waiter = KthreadIrqWaiter::new(&coop, || arch.monotonic_ns(cpu));
/// match block_until_ready(table, irq_handle, owner, u64::MAX, &waiter) {
///     WaitOutcome::Ready => { /* the device line fired */ }
///     other => { /* timeout / forged / aborted — fail closed */ }
/// }
/// ```
///
/// # Why a shared [`CooperativeYield`]
///
/// A kthread that drives interrupt-driven block I/O **and** reads the
/// console (the INCREMENT (2) root-unlock kthread) needs *two* things to
/// suspend on the same `&mut dyn YieldHandle`: this waiter (inside the
/// virtio host's `notify_wait`) and a cooperative console reader, both
/// alive for the whole unlock call. A single `&mut` cannot be owned by
/// two of them, so the borrowed handle is shared through a
/// [`CooperativeYield`] cell both borrow (`&`), each suspending through
/// it transiently. The waiter therefore holds `&CooperativeYield`, not the
/// `&mut` itself.
pub struct KthreadIrqWaiter<'a, C>
where
    C: Fn() -> u64,
{
    /// The kthread's shared cooperative-yield cell (the core's
    /// [`YielderHandle`](crate::kthread::YielderHandle) behind the
    /// object-safe trait, wrapped for shared suspension).
    yielder: &'a CooperativeYield<'a>,
    /// Monotonic clock, in nanoseconds, on the kthread's CPU. Closing over
    /// the arch + CPU keeps the waiter free of any architecture
    /// dependency, mirroring `SyscallIrqWaiter`'s `KernelArch` borrow.
    clock: C,
}

impl<'a, C> KthreadIrqWaiter<'a, C>
where
    C: Fn() -> u64,
{
    /// Adapt the shared [`CooperativeYield`] cell and the monotonic
    /// `clock` into an [`IrqWaiter`].
    #[must_use]
    pub fn new(yielder: &'a CooperativeYield<'a>, clock: C) -> Self {
        Self { yielder, clock }
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
        self.yielder.yield_now();
        Ok(())
    }
}

/// A shared cooperative-yield cell over a kthread's single
/// `&mut dyn YieldHandle`.
///
/// An in-kernel service kthread is handed exactly one
/// [`YieldHandle`] by the core, but may need to suspend from more than one
/// place that is alive at the same time — the [`KthreadIrqWaiter`] inside a
/// block driver's `notify_wait` *and* a cooperative console reader during
/// the same root-unlock call. A single `&mut` cannot be shared, so the
/// handle is moved into this cell once and every suspending site borrows
/// the cell (`&`), suspending through [`Self::yield_now`].
///
/// # Interior mutability
///
/// [`YieldHandle::yield_now`] needs `&mut self` (it suspends the running
/// coroutine), but the cell is shared by `&`, so the handle is wrapped in a
/// [`RefCell`]. This is sound and panic-free: a kthread runs on at most one
/// CPU and the suspending sites call [`Self::yield_now`] strictly serially,
/// never re-entrantly (each `borrow_mut` is released the instant the inner
/// `yield_now` returns), so the borrow is never already held. It is *not* a
/// global mutable static (`AGENTS.md` §2.1) — it lives on the kthread's own
/// stack frame and is `!Sync` (never shared across CPUs).
pub struct CooperativeYield<'a> {
    yielder: RefCell<&'a mut dyn YieldHandle>,
}

impl<'a> CooperativeYield<'a> {
    /// Move the kthread's single [`YieldHandle`] into a shareable cell.
    #[must_use]
    pub fn new(yielder: &'a mut dyn YieldHandle) -> Self {
        Self {
            yielder: RefCell::new(yielder),
        }
    }

    /// Cooperatively yield the kthread, resuming on its next dispatch.
    ///
    /// The `borrow_mut` is held only for the duration of the inner
    /// [`YieldHandle::yield_now`] call; because the kthread's suspending
    /// sites never call this re-entrantly, the borrow is never already
    /// held.
    pub fn yield_now(&self) {
        self.yielder.borrow_mut().yield_now();
    }

    /// Park the kthread until an external `unpark` re-enqueues it, then
    /// resume on its next dispatch (see [`YieldHandle::park`]).
    ///
    /// Unlike [`Self::yield_now`], a parked task is *not* re-enqueued, so it
    /// consumes no CPU while suspended — this is the suspension a long-lived
    /// kernel service uses to wait for work without busy-yielding
    /// (`AGENTS.md` §2.1). The `borrow_mut` is held only for the inner
    /// [`YieldHandle::park`] call; the kthread's suspending sites never call
    /// this re-entrantly, so the borrow is never already held.
    pub fn park(&self) {
        self.yielder.borrow_mut().park();
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
        let coop = CooperativeYield::new(&mut mock);
        let waiter = KthreadIrqWaiter::new(&coop, || 0);

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
        let coop = CooperativeYield::new(&mut mock);
        let waiter = KthreadIrqWaiter::new(&coop, || 0);

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
        let coop = CooperativeYield::new(&mut mock);
        let waiter = KthreadIrqWaiter::new(&coop, clock);

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
        let coop = CooperativeYield::new(&mut mock);
        let waiter = KthreadIrqWaiter::new(&coop, || 0);

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
        let coop = CooperativeYield::new(&mut mock);
        let waiter = KthreadIrqWaiter::new(&coop, || 0xABCD_1234);
        assert_eq!(waiter.now_ns(), 0xABCD_1234);
    }

    /// A mock [`YieldHandle`] that counts `park` calls (and would record a
    /// stray `yield_now`), so [`CooperativeYield::park`]'s delegation to the
    /// inner handle's `park` is observable.
    struct ParkRecorder<'a> {
        parks: &'a Cell<u32>,
    }

    impl YieldHandle for ParkRecorder<'_> {
        fn yield_now(&mut self) {
            panic!("CooperativeYield::park must delegate to park, never yield_now");
        }

        fn park(&mut self) {
            self.parks.set(self.parks.get() + 1);
        }
    }

    #[test]
    fn cooperative_park_delegates_to_the_inner_handle_park() {
        // A long-lived kernel service (the Design D D2a-2 driver-store
        // service) parks through the shared cell rather than busy-yielding
        // (`AGENTS.md` §2.1): `park` must reach the inner handle's `park`.
        let parks = Cell::new(0);
        let mut mock = ParkRecorder { parks: &parks };
        let coop = CooperativeYield::new(&mut mock);
        coop.park();
        coop.park();
        assert_eq!(parks.get(), 2, "each park delegates exactly once");
    }
}
