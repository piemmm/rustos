//! Bounded data-parallel work: how a pass hands the machine's other cores the
//! work it has already proved independent.
//!
//! A pass that has split its output into pieces no two of which read or write
//! the same byte — a compositor's row bands, a blur's column sub-bands — states
//! that by handing the pieces here. Nothing in this crate discovers
//! independence; it is the caller's proof, and the caller keeps it.
//!
//! # The three parts
//!
//! * [`JobRunner`] — the contract. A pass names it, so the pass compiles and
//!   runs without a thread anywhere near it: [`Serial`] is a runner, and it is
//!   the whole of what an in-kernel or single-core consumer links.
//! * [`for_each`] — the one place an index becomes an element. A runner deals
//!   in indices because that is all it can share between threads; a pass wants
//!   its own `&mut` piece. That conversion is the crate's single `unsafe`
//!   block, and it lives here rather than in every pass.
//! * [`Pool`] (feature `pool`) — the fork-join worker pool over `lib/rt`
//!   threads. Workers park on a futex between dispatches and never spin.
//!
//! # Sizing
//!
//! [`bands`] is the one split policy. A caller says how many units of work it
//! has and how few units are worth a hand-off; the answer is how many pieces to
//! make. Work below one piece's worth runs on the calling thread with no
//! atomics and no syscall, so a small repaint costs exactly what it did before
//! a pool existed.
//!
//! [`Pool`]: pool::Pool

#![no_std]

#[cfg(feature = "pool")]
extern crate alloc;

#[cfg(feature = "pool")]
pub mod pool;

#[cfg(feature = "pool")]
pub use pool::Pool;

/// How many pieces of work a runner can make progress on at once, and how to
/// run them.
///
/// # Safety
///
/// [`for_each`] hands each index an exclusive borrow of one element, so an
/// implementation must guarantee both of:
///
/// 1. **Each index is passed to `job` at most once, and never concurrently with
///    itself.** Two live invocations for the same index would alias one `&mut`.
/// 2. **`run` does not return until every invocation it made has returned.**
///    The caller's borrow of the elements ends when `run` returns, so an
///    invocation still running past it would hold a dangling reference.
///
/// Indices `job` is *not* passed are the implementation's business: a runner
/// that skips one simply leaves that element unvisited, which is a correctness
/// bug in the runner and not unsoundness. Both obligations above are memory
/// safety, which is why this trait is `unsafe`.
pub unsafe trait JobRunner: Sync {
    /// How many of this runner's jobs can be in progress at once — `1` for a
    /// runner that runs them on the calling thread.
    ///
    /// A caller splits its work into [`bands`] pieces derived from this, so a
    /// runner reporting `1` is asked for one piece and pays nothing for the
    /// machinery it does not use.
    fn width(&self) -> usize;

    /// Run `job(index)` for every `index` in `0..count`, in any order and
    /// possibly concurrently, returning once every one of them has returned.
    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync));
}

/// The runner that runs every job on the calling thread, in index order.
///
/// This is not a fallback or a stub: it is the correct runner wherever there is
/// no second core to use, no thread to create (an in-kernel consumer), or no
/// reason to hand work off. A pass written against [`JobRunner`] is complete
/// with only this.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Serial;

// SAFETY: `run` calls `job` exactly once for each index of `0..count`, from the
// calling thread, and the loop has finished before it returns — so no index is
// ever live twice and nothing outlives the call.
unsafe impl JobRunner for Serial {
    fn width(&self) -> usize {
        1
    }

    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        for index in 0..count {
            job(index);
        }
    }
}

/// The [`Serial`] runner, for a caller that needs a `&'static dyn JobRunner`
/// and has no pool.
pub static SERIAL: Serial = Serial;

/// How much more finely than [`JobRunner::width`] work is split.
///
/// Pieces are claimed dynamically, so splitting finer than the runner is wide
/// costs one atomic increment per extra piece and buys back the case that
/// actually bites on a loaded machine: a core taken by another tenant leaves
/// one participant late, and with a piece each the whole pass waits for it.
/// With several pieces each, a straggler holds up one small piece and the
/// others absorb its share.
const OVERSUBSCRIPTION: usize = 4;

/// How many pieces `units` units of work should be split into for `runner`,
/// where a piece carrying fewer than `grain` units is not worth handing off.
///
/// `0` for no work at all, `1` whenever the work is too small to split or the
/// runner is one thread wide — in which case a caller runs its loop exactly as
/// it would with no pool at all.
#[must_use]
pub fn bands(runner: &dyn JobRunner, units: usize, grain: usize) -> usize {
    if units == 0 {
        return 0;
    }
    let width = runner.width().max(1);
    if width == 1 {
        return 1;
    }
    // Whole pieces of at least `grain` units, so the last piece is the only one
    // that can be short and a tiny job is never fragmented.
    let affordable = units / grain.max(1);
    affordable.clamp(1, width.saturating_mul(OVERSUBSCRIPTION))
}

/// A raw pointer to the elements of one [`for_each`] call, shared with that
/// call's jobs.
///
/// Private and reachable only from [`for_each`], so the `Send` / `Sync` claims
/// below cannot be borrowed by anything that does not hold [`JobRunner`]'s
/// obligations. The accessor exists so a job closure captures the whole wrapper
/// rather than the bare pointer field, which is what carries those claims into
/// the closure's own auto-traits.
struct Elements<T>(*mut T);

impl<T> Elements<T> {
    /// The address of element `index`.
    const fn at(&self, index: usize) -> *mut T {
        // SAFETY: the caller checks `index` against the slice's length before
        // dereferencing; forming the address itself stays in bounds because of
        // that check, and this returns a raw pointer rather than a borrow.
        unsafe { self.0.add(index) }
    }
}

// SAFETY: the pointer is only ever offset to an index the runner passed to
// exactly one live job, so the access it grants is an exclusive borrow of one
// `T` — sending that between threads is sending a `T`.
unsafe impl<T: Send> Send for Elements<T> {}
// SAFETY: as above; sharing the pointer between jobs grants each of them a
// different element.
unsafe impl<T: Send> Sync for Elements<T> {}

/// Visit each element of `items` exactly once — `visit(&mut items[i])` for every
/// `i` — spread across `runner`, returning when every element has been visited.
///
/// This is the crate's whole surface for a pass: the pass splits its output into
/// `items` (whose disjointness it proved by construction, typically with
/// `split_at_mut` / `chunks_mut`), and this hands each piece to whichever
/// participant claims it.
///
/// An empty slice does nothing and a single element is visited on the calling
/// thread, so neither reaches the runner.
pub fn for_each<T: Send>(runner: &dyn JobRunner, items: &mut [T], visit: &(dyn Fn(&mut T) + Sync)) {
    let count = items.len();
    if count <= 1 {
        if let Some(only) = items.first_mut() {
            visit(only);
        }
        return;
    }
    let elements = Elements(items.as_mut_ptr());
    runner.run(count, &|index| {
        // A runner that hands out an index it was never given would be unsound
        // rather than merely wrong, so the bound is re-checked here: the worst a
        // broken runner can then do is leave an element unvisited.
        if index >= count {
            return;
        }
        // SAFETY: `index < count`, so the address is inside the slice the caller
        // still borrows exclusively; `JobRunner` guarantees no other live job
        // holds this index, so the borrow is unique; and `run` does not return
        // until this job has, so the slice outlives it.
        let item = unsafe { &mut *elements.at(index) };
        visit(item);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runner that reports a width it does not have and runs the jobs
    /// backwards on the calling thread: enough to prove a pass's output does not
    /// depend on the order its pieces run in, without needing a real thread.
    struct Reversed(usize);

    // SAFETY: each index of `0..count` is passed exactly once, from the calling
    // thread, and every call has returned before `run` does.
    unsafe impl JobRunner for Reversed {
        fn width(&self) -> usize {
            self.0
        }

        fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
            for index in (0..count).rev() {
                job(index);
            }
        }
    }

    /// A runner that drops the last job, so the "an unvisited element is a bug,
    /// not unsoundness" claim is exercised rather than only argued.
    struct Forgetful;

    // SAFETY: the indices it does pass are each passed once, on the calling
    // thread, and all have returned before `run` does. Skipping one is
    // permitted.
    unsafe impl JobRunner for Forgetful {
        fn width(&self) -> usize {
            2
        }

        fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
            for index in 0..count.saturating_sub(1) {
                job(index);
            }
        }
    }

    #[test]
    fn each_element_is_visited_exactly_once() {
        let mut items = [0u32; 16];
        for_each(&SERIAL, &mut items, &|item| *item += 1);
        assert!(items.iter().all(|&count| count == 1));
    }

    /// The property every parallel pass depends on: the result cannot depend on
    /// which piece runs first.
    #[test]
    fn the_order_pieces_run_in_does_not_change_the_result() {
        let mut forwards = [0u32; 16];
        let mut backwards = [0u32; 16];
        let stamp = |slot: &mut u32| *slot = *slot * 2 + 1;
        for_each(&SERIAL, &mut forwards, &stamp);
        for_each(&Reversed(4), &mut backwards, &stamp);
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn an_empty_slice_visits_nothing_and_one_element_is_visited_on_the_caller() {
        let mut nothing: [u32; 0] = [];
        for_each(&Reversed(4), &mut nothing, &|_| {
            panic!("no element to visit")
        });

        let mut one = [7u32];
        for_each(&Reversed(4), &mut one, &|item| *item += 1);
        assert_eq!(one, [8]);
    }

    /// A runner that skips a piece leaves that element alone. It is a defect in
    /// the runner, and the point here is that it is not memory-unsafe: the test
    /// runs clean under Miri-style aliasing rules because no borrow escapes.
    #[test]
    fn a_runner_that_skips_a_piece_leaves_that_element_unvisited() {
        let mut items = [0u32; 4];
        for_each(&Forgetful, &mut items, &|item| *item += 1);
        assert_eq!(items, [1, 1, 1, 0]);
    }

    #[test]
    fn no_work_is_split_into_no_pieces() {
        assert_eq!(bands(&Reversed(8), 0, 1), 0);
        assert_eq!(bands(&SERIAL, 0, 1), 0);
    }

    #[test]
    fn a_one_thread_wide_runner_is_always_asked_for_one_piece() {
        assert_eq!(bands(&SERIAL, 1_000_000, 1), 1);
    }

    #[test]
    fn work_below_one_grain_is_not_split() {
        assert_eq!(bands(&Reversed(8), 15, 16), 1);
        assert_eq!(bands(&Reversed(8), 16, 16), 1);
        assert_eq!(bands(&Reversed(8), 31, 16), 1);
        assert_eq!(bands(&Reversed(8), 32, 16), 2);
    }

    #[test]
    fn the_split_never_exceeds_the_runner_s_oversubscribed_width() {
        let runner = Reversed(4);
        assert_eq!(
            bands(&runner, 1_000_000, 1),
            4 * OVERSUBSCRIPTION,
            "however much work there is, the pieces are bounded by the runner"
        );
    }

    /// A zero grain would divide by zero; it reads as one unit per piece.
    #[test]
    fn a_zero_grain_reads_as_one_unit_per_piece() {
        assert_eq!(bands(&Reversed(2), 3, 0), 3);
    }

    /// A runner claiming an absurd width must not overflow the bound.
    #[test]
    fn an_absurd_width_still_yields_a_usable_split() {
        assert_eq!(bands(&Reversed(usize::MAX), 10, 1), 10);
    }
}
