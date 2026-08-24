//! Threads within one process: [`Thread::spawn`] and [`JoinHandle`]
//! (`plans/THREADS.md` T3b).
//!
//! A thread here is a second flow of control over the *same* heap, the same
//! descriptors, and the same capability record — the kernel's `thread_create`,
//! wrapped so a program hands over a closure and gets a value back instead of
//! marshalling registers itself.
//!
//! # Who owns what
//!
//! The **kernel** owns a thread's stack: it reserves the stack and the unbacked
//! guard page below it out of the process's own anonymous window and releases
//! the whole region when the thread dies (decision 5a). So this runtime owns no
//! stack memory, a stack overrun faults deterministically instead of walking
//! into a neighbouring mapping, and a detached thread cannot leak its stack.
//!
//! This runtime owns two things per thread: the **payload** (the closure on the
//! way in, boxed and freed by the thread itself as its first act) and the
//! **rendezvous cell** (the join word and the outcome slot).
//!
//! # Why cells are recycled rather than freed
//!
//! A cell's `alive` word is the address handed to the kernel as
//! `clear_on_exit`: the kernel zeroes it and futex-wakes it at thread death,
//! and there is no way to retract that. Returning the cell's memory to the heap
//! while its thread still lives would let the kernel zero four bytes of
//! whatever came to occupy them. Cells are therefore never freed — they are
//! **recycled**, and only once their word reads zero, which is exactly the
//! proof that the kernel's one write has already happened. That is what makes a
//! detached thread cost nothing permanent: its cell returns to the registry and
//! the next `spawn` reuses it.
//!
//! # Joining is kernel-witnessed
//!
//! `join` waits on the kernel's word, not on a store the dying thread makes, so
//! a thread that is *fault-killed* — one that never reaches its own epilogue —
//! still releases its joiner. The joiner then observes that no value was
//! published and reports [`JoinError::Died`] rather than waiting forever.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use tairix_abi::{Errno, THREAD_STACK_DEFAULT};

use crate::sync::Mutex;

/// [`Rendezvous::alive`]: the thread is running. The kernel replaces it with
/// zero when the thread dies.
const ALIVE: u32 = 1;

/// [`Rendezvous::alive`]: the kernel has recorded the thread's death.
const DEAD: u32 = 0;

/// [`Rendezvous::state`]: both the handle and the thread are still interested.
const JOINABLE: u32 = 0;
/// [`Rendezvous::state`]: the handle is gone, so the thread owns the outcome.
const DETACHED: u32 = 1;
/// [`Rendezvous::state`]: the thread published its outcome, so the handle owns
/// it.
const FINISHED: u32 = 2;

/// A thread's identity: the kernel task id `thread_create` returned.
///
/// Ids are never reused, so a `ThreadId` names one thread for the lifetime of
/// the system and can never be mistaken for a later one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    /// The raw kernel task id, for a diagnostic or a syscall that names a
    /// thread.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Why a [`JoinHandle::join`] could not produce the thread's value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JoinError {
    /// The thread died without producing a value: it was fault-killed or
    /// terminated part-way through its body. Its stack and kernel state are
    /// reclaimed either way, so this is a report, not a leak.
    Died,
    /// The *joining* thread is itself being terminated, so its wait was
    /// interrupted rather than completed.
    Interrupted,
}

/// The per-thread rendezvous cell: the join word the kernel writes, plus the
/// handshake and the slot the outcome travels through.
///
/// Never freed, only recycled ([`Registry`]).
struct Rendezvous {
    /// The kernel's `clear_on_exit` word — [`ALIVE`] from creation until the
    /// kernel stores [`DEAD`] at thread death and futex-wakes it. Its address
    /// crosses into the kernel, so it must stay a naturally aligned `u32` at a
    /// stable location.
    alive: AtomicU32,
    /// Which side owns the outcome: [`JOINABLE`] until the handle detaches
    /// ([`DETACHED`]) or the thread publishes ([`FINISHED`]). Whoever loses that
    /// race drops the value, so it is dropped exactly once.
    state: AtomicU32,
    /// The thread's boxed return value, published *before* `state` becomes
    /// [`FINISHED`]. Null means "no value was ever published".
    outcome: AtomicPtr<u8>,
}

impl Rendezvous {
    /// A cell for a thread that is about to be created.
    const fn new() -> Self {
        Self {
            alive: AtomicU32::new(ALIVE),
            state: AtomicU32::new(JOINABLE),
            outcome: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Re-arm a recycled cell for a fresh thread.
    fn rearm(&self) {
        self.outcome.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.state.store(JOINABLE, Ordering::Relaxed);
        // Published last, with `Release`: it is the word the kernel and the
        // joiner both read, so the reset state above must be visible first.
        self.alive.store(ALIVE, Ordering::Release);
    }

    /// Whether the kernel has recorded this cell's thread as dead.
    fn thread_is_dead(&self) -> bool {
        self.alive.load(Ordering::Acquire) == DEAD
    }
}

/// The process's rendezvous-cell registry.
///
/// `free` holds cells whose rendezvous is complete *and* whose thread the
/// kernel has recorded as dead, so they are reusable immediately. `retired`
/// holds detached cells still awaiting that record; every acquisition sweeps
/// them, which is one atomic load per retired cell — bounded by the process's
/// live thread count, itself bounded by the `threads` resource limit.
struct Registry {
    free: Vec<&'static Rendezvous>,
    retired: Vec<&'static Rendezvous>,
}

/// The one registry. Guarded by the runtime's own futex mutex, so a contended
/// spawn parks rather than spinning.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    free: Vec::new(),
    retired: Vec::new(),
});

/// Take a re-armed cell for a new thread, recycling a retired one where the
/// kernel's write has landed and leaking a fresh one otherwise.
fn acquire_cell() -> &'static Rendezvous {
    let recycled = {
        let mut registry = REGISTRY.lock();
        // Sweep the retired cells first: a detached thread that has since died
        // returns its cell to circulation, so a spawn/detach cycle allocates
        // exactly once however many times it repeats.
        let mut index = 0;
        while index < registry.retired.len() {
            if registry.retired[index].thread_is_dead() {
                let cell = registry.retired.swap_remove(index);
                registry.free.push(cell);
            } else {
                index += 1;
            }
        }
        registry.free.pop()
    };
    match recycled {
        Some(cell) => {
            cell.rearm();
            cell
        }
        // Leaked deliberately: the kernel holds this cell's `alive` address for
        // the thread's lifetime, and the registry hands the storage out again
        // rather than ever returning it to the heap.
        None => Box::leak(Box::new(Rendezvous::new())),
    }
}

/// Return a cell whose thread is provably gone and whose rendezvous is
/// complete, so the next spawn may reuse it immediately.
fn release_cell(cell: &'static Rendezvous) {
    REGISTRY.lock().free.push(cell);
}

/// Hand a detached cell back for later recycling: its thread may still be
/// running, so it becomes reusable only once the kernel's write lands.
fn retire_cell(cell: &'static Rendezvous) {
    REGISTRY.lock().retired.push(cell);
}

/// The monomorphised body of a thread, read from the head of its payload by the
/// one type-erased entry point.
type Runner = unsafe extern "C" fn(payload: *mut u8) -> !;

/// A thread's payload: the runner that knows the closure's type, the cell the
/// two sides rendezvous through, and the closure itself.
///
/// `#[repr(C)]` with `run` first so the one type-erased entry — which cannot
/// know `F` or `T` — reads the runner from the payload's own address.
#[repr(C)]
struct Payload<F, T> {
    run: Runner,
    cell: &'static Rendezvous,
    body: F,
    /// Ties the payload to the outcome type the runner boxes, so a payload can
    /// never be run by a runner expecting a different one.
    _outcome: PhantomData<fn() -> T>,
}

/// Run one thread's closure and end the thread. The monomorphised half of the
/// entry path: it alone knows `F` and `T`.
///
/// # Safety
///
/// `payload` must be the `Box<Payload<F, T>>` pointer `Thread::spawn` handed
/// the kernel for *this* thread, not yet consumed.
unsafe extern "C" fn run<F, T>(payload: *mut u8) -> !
where
    F: FnOnce() -> T,
{
    // SAFETY: the caller guarantees this is the live payload box for this
    // thread; reclaiming it here frees the closure's storage as the thread's
    // first act, so a detached thread holds no allocation while it runs.
    let payload = unsafe { Box::from_raw(payload.cast::<Payload<F, T>>()) };
    let Payload { cell, body, .. } = *payload;

    let value = body();

    // Publish the value *before* the handshake: the handle reads the slot only
    // after seeing `FINISHED`, so the store cannot be missed.
    let boxed: *mut u8 = Box::into_raw(Box::new(value)).cast::<u8>();
    cell.outcome.store(boxed, Ordering::Release);
    if cell.state.swap(FINISHED, Ordering::AcqRel) == DETACHED {
        // Nobody will ever take it: the handle detached before we published.
        // Taking the pointer back out makes the drop unrepeatable.
        let orphan = cell.outcome.swap(core::ptr::null_mut(), Ordering::Acquire);
        if !orphan.is_null() {
            // SAFETY: `orphan` is the `Box<T>` published a moment ago and taken
            // out exactly once, so this reconstitutes a live, unaliased box.
            drop(unsafe { Box::from_raw(orphan.cast::<T>()) });
        }
    }

    // The kernel zeroes and futex-wakes `cell.alive` from here, which is what
    // releases a joiner — including for a thread that never got this far.
    crate::thread_exit()
}

/// The one type-erased thread entry, reached from the per-architecture
/// alignment trampoline. Reads the monomorphised runner out of the payload's
/// head and hands the payload to it.
///
/// # Safety
///
/// `payload` must be a live `Payload<F, T>` box whose `run` field is
/// `run::<F, T>`; `Thread::spawn` builds exactly that.
#[cfg_attr(rt_native, unsafe(no_mangle))]
#[allow(clippy::cast_ptr_alignment)] // `payload` came from `Box::into_raw` of a `Payload<_, _>`, whose first field is this very fn pointer, so it is at least fn-pointer aligned.
unsafe extern "C" fn __tairix_rt_thread_start(payload: *mut u8) -> ! {
    // SAFETY: the caller guarantees `payload` points at a `Payload<_, _>`, whose
    // `#[repr(C)]` layout puts the runner at offset zero.
    let run = unsafe { core::ptr::read(payload.cast::<Runner>()) };
    // SAFETY: `run` is the monomorphised runner for this very payload, so the
    // types it recovers are the ones the payload was built with.
    unsafe { run(payload) }
}

// The per-architecture thread-entry trampoline — the assembly carve-out.
//
// Justification: the kernel enters a new thread with a page-aligned stack
// pointer and the payload in the C integer-argument-0 register, exactly as it
// enters a process at `_start`. On x86_64 the SysV ABI defines a function's
// entry alignment *relative to a `call`* (the return address has been pushed),
// so a compiled Rust function entered by a direct jump would mis-place every
// 16-byte-aligned spill slot. Aligning the stack and reaching the Rust entry
// through a real `call` has no Rust spelling, so each trampoline does only
// that. This is the same carve-out `_start` documents in `start.rs`, applied to
// a thread's first instruction; the register the payload arrives in is already
// argument 0 on all three targets, so nothing is moved.
#[cfg(rt_native_x86_64)]
core::arch::global_asm!(
    ".global __tairix_rt_thread_entry",
    "__tairix_rt_thread_entry:",
    "and rsp, -16",
    "call __tairix_rt_thread_start",
    "ud2",
);

#[cfg(rt_native_aarch64)]
core::arch::global_asm!(
    ".global __tairix_rt_thread_entry",
    "__tairix_rt_thread_entry:",
    "mov x9, sp",
    "and x9, x9, #-16",
    "mov sp, x9",
    "bl __tairix_rt_thread_start",
    "brk #0",
);

#[cfg(rt_native_riscv64)]
core::arch::global_asm!(
    ".global __tairix_rt_thread_entry",
    "__tairix_rt_thread_entry:",
    "andi sp, sp, -16",
    "call __tairix_rt_thread_start",
    "ebreak",
);

#[cfg(rt_native)]
extern "C" {
    /// The trampoline above, whose address is what `thread_create` is given as
    /// the new thread's entry point.
    fn __tairix_rt_thread_entry(payload: *mut u8) -> !;
}

/// The address the kernel starts a new thread at.
///
/// On a native target this is the alignment trampoline. On the host there is no
/// kernel and no trap, so `thread_create` fails closed before the address is
/// ever used; naming the type-erased entry keeps the symbol live and the
/// bookkeeping host-testable.
fn thread_entry_address() -> u64 {
    #[cfg(rt_native)]
    let entry = __tairix_rt_thread_entry as unsafe extern "C" fn(*mut u8) -> !;
    #[cfg(not(rt_native))]
    let entry = __tairix_rt_thread_start as unsafe extern "C" fn(*mut u8) -> !;
    entry as usize as u64
}

/// A handle naming one thread of this process.
///
/// It carries identity only: a thread is not controlled through this handle,
/// because there is no syscall that reaches into another thread — a program
/// coordinates with its threads through [`crate::sync`] and the join
/// rendezvous.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Thread {
    id: ThreadId,
}

impl Thread {
    /// This thread's id.
    #[must_use]
    pub const fn id(self) -> ThreadId {
        self.id
    }

    /// Start a thread running `body` in this process's own address space,
    /// returning a handle that yields its value.
    ///
    /// The default stack size and thread pointer apply; [`Builder`] names either
    /// explicitly.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the kernel refused `thread_create` with — `OutOfRange` at
    /// the `threads` or `stack-bytes` bound, `OutOfMemory` when the anonymous
    /// window or the frame allocator is exhausted, `NotImplemented` on a build
    /// with no user address space (the host, `wasm32`).
    pub fn spawn<F, T>(body: F) -> Result<JoinHandle<T>, Errno>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Builder::new().spawn(body)
    }
}

/// How a thread is to be created: the two parameters `thread_create` takes
/// beyond the body itself.
///
/// Both have a default a program never has to think about, so the common case
/// is [`Thread::spawn`]. Naming one is for a program that genuinely needs to:
/// many small threads on a machine whose `stack-bytes` bound is generous, or a
/// thread pointer aimed at the program's own thread-local block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Builder {
    stack_bytes: usize,
    thread_pointer: Option<u64>,
}

impl Builder {
    /// A builder with both parameters at their defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stack_bytes: THREAD_STACK_DEFAULT,
            thread_pointer: None,
        }
    }

    /// Reserve `bytes` of user stack instead of the caller's whole
    /// `stack-bytes` bound.
    ///
    /// The kernel page-rounds the request and refuses one *above* that bound, so
    /// this is a way to ask for less. [`tairix_abi::THREAD_STACK_DEFAULT`]
    /// restores the default.
    #[must_use]
    pub const fn stack_bytes(mut self, bytes: usize) -> Self {
        self.stack_bytes = bytes;
        self
    }

    /// Point the thread's psABI thread pointer (`TPIDR_EL0` / `tp` /
    /// `IA32_FS_BASE`) at `base` instead of its rendezvous cell.
    ///
    /// The default already makes the register per-thread and valid, which is the
    /// kernel's contract. A program that lays out its own thread-local block
    /// names it here so accesses *through* the register reach that block; `base`
    /// must be readable memory of this process, or the kernel refuses the
    /// creation.
    #[must_use]
    pub const fn thread_pointer(mut self, base: u64) -> Self {
        self.thread_pointer = Some(base);
        self
    }

    /// Start a thread running `body` under these parameters.
    ///
    /// # Errors
    ///
    /// As [`Thread::spawn`]; additionally `LengthOutOfRange` for a stack request
    /// that rounds to nothing and `BadAddress` for a thread pointer outside this
    /// process's own memory.
    pub fn spawn<F, T>(self, body: F) -> Result<JoinHandle<T>, Errno>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let cell = acquire_cell();
        let payload = Box::into_raw(Box::new(Payload::<F, T> {
            run: run::<F, T>,
            cell,
            body,
            _outcome: PhantomData,
        }))
        .cast::<u8>();
        // The rendezvous cell is the thread's control block by default: it is
        // per-thread, at a stable address, and — unlike the payload, which the
        // thread frees as its first act — it outlives the thread. That makes the
        // thread psABI-conforming (its thread pointer names live memory of this
        // process) before any thread-local storage layer exists.
        let thread_pointer = self
            .thread_pointer
            .unwrap_or_else(|| core::ptr::from_ref(cell) as usize as u64);

        // SAFETY: `thread_entry_address` names the trampoline, which never
        // returns (it ends in `run`'s `thread_exit`); `payload` is the live box
        // built above, handed over exactly once; `cell.alive` is a naturally
        // aligned `u32` in a cell this runtime never frees, so it outlives the
        // thread whose death the kernel records there.
        let created = unsafe {
            crate::thread_create(
                thread_entry_address(),
                payload as usize as u64,
                self.stack_bytes,
                thread_pointer,
                core::ptr::from_ref(&cell.alive) as usize as u64,
            )
        };
        let Ok(id) = u64::try_from(created) else {
            // No thread exists, so nothing will ever write the cell's word:
            // mark it dead and return both the cell and the payload.
            cell.alive.store(DEAD, Ordering::Release);
            release_cell(cell);
            // SAFETY: the kernel refused the creation, so no thread took the
            // payload; this reclaims the box built above, unconsumed.
            drop(unsafe { Box::from_raw(payload.cast::<Payload<F, T>>()) });
            return Err(Errno::from_syscall(created));
        };
        Ok(JoinHandle {
            thread: Thread { id: ThreadId(id) },
            cell,
            _outcome: PhantomData,
        })
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// The owning handle to a spawned thread: joining it yields the closure's
/// value, and dropping it detaches the thread.
///
/// Dropping is *detaching*, not cancelling: there is no way to cancel a thread
/// mid-flight, and inventing one would leave the process's shared state
/// arbitrarily half-mutated. A detached thread runs to completion and the
/// kernel reclaims it.
pub struct JoinHandle<T> {
    thread: Thread,
    cell: &'static Rendezvous,
    _outcome: PhantomData<T>,
}

// SAFETY: the handle's only interior state is the rendezvous cell, whose every
// field is an atomic, and the value it carries out is a `T` the thread produced
// — so moving a handle between threads is sound exactly when moving the value
// is.
unsafe impl<T: Send> Send for JoinHandle<T> {}
// SAFETY: as above; every method takes `&mut self` or `self`, so sharing a
// handle exposes only the immutable `thread` identity.
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    /// The thread this handle names.
    #[must_use]
    pub const fn thread(&self) -> Thread {
        self.thread
    }

    /// Block until the thread ends and take the value its closure returned.
    ///
    /// The wait is on the word the **kernel** zeroes at thread death, so a
    /// thread that was fault-killed part-way through its body still releases the
    /// joiner — it reports [`JoinError::Died`] instead of the value.
    ///
    /// # Errors
    ///
    /// [`JoinError::Died`] when the thread ended without producing a value, and
    /// [`JoinError::Interrupted`] when the joining thread is itself being
    /// terminated.
    pub fn join(self) -> Result<T, JoinError> {
        let cell = self.cell;
        // The handle's own teardown must not also detach: `join` decides what
        // happens to the cell and the outcome from here.
        core::mem::forget(self);

        loop {
            let observed = cell.alive.load(Ordering::Acquire);
            if observed == DEAD {
                break;
            }
            // SAFETY: `alive` is a naturally aligned `u32` in a cell this
            // runtime never frees, so it is live for the call. A refusal is a
            // re-test, not a failure: `WouldBlock` means the word already
            // changed, and a spurious wake costs one loop.
            let outcome = unsafe { crate::futex_wait(word_of(&cell.alive), observed, u64::MAX) };
            if outcome < 0 && Errno::from_syscall(outcome) == Errno::Interrupted {
                // This thread is being terminated; do not loop against a wait
                // that can no longer complete. The cell is left retired so the
                // kernel's pending write has somewhere valid to land.
                retire_cell(cell);
                return Err(JoinError::Interrupted);
            }
        }

        // The thread is gone. Its rendezvous is complete either way, so the cell
        // is immediately reusable.
        let published = cell.outcome.swap(core::ptr::null_mut(), Ordering::Acquire);
        let finished = cell.state.load(Ordering::Acquire) == FINISHED;
        release_cell(cell);
        if !finished || published.is_null() {
            return Err(JoinError::Died);
        }
        // SAFETY: the thread published exactly one `Box<T>` before marking
        // itself finished, and the swap above took it exactly once.
        Ok(*unsafe { Box::from_raw(published.cast::<T>()) })
    }

    /// Give up the right to join, letting the thread run to completion on its
    /// own.
    ///
    /// The same thing dropping the handle does, spelled out where a reader would
    /// otherwise wonder whether the value was forgotten by accident.
    pub fn detach(self) {
        drop(self);
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // Whoever wins this swap owns the outcome. Losing it to the thread means
        // the thread has already published and will not drop it, so the handle
        // must.
        if self.cell.state.swap(DETACHED, Ordering::AcqRel) == FINISHED {
            let orphan = self
                .cell
                .outcome
                .swap(core::ptr::null_mut(), Ordering::Acquire);
            if !orphan.is_null() {
                // SAFETY: the thread published one `Box<T>`; this swap took it
                // exactly once, and the thread's own `swap` saw `JOINABLE` and
                // therefore did not.
                drop(unsafe { Box::from_raw(orphan.cast::<T>()) });
            }
        }
        // The thread may still be running, so the cell's word must stay valid
        // for the kernel's write: retire it rather than freeing or reusing it.
        retire_cell(self.cell);
    }
}

/// The address of a rendezvous word, in the form the futex syscalls take.
fn word_of(word: &AtomicU32) -> u64 {
    core::ptr::from_ref(word) as usize as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so the cell tests serialise on it: they
    /// assert on counts, which a parallel test moving cells would perturb.
    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn a_fresh_cell_starts_alive_joinable_and_empty() {
        let _g = registry_lock();
        let cell = acquire_cell();
        assert_eq!(cell.alive.load(Ordering::Relaxed), ALIVE);
        assert_eq!(cell.state.load(Ordering::Relaxed), JOINABLE);
        assert!(cell.outcome.load(Ordering::Relaxed).is_null());
        assert!(!cell.thread_is_dead());
    }

    /// A cell whose thread is provably gone is reused rather than reallocated —
    /// what keeps a spawn/join cycle from leaking one cell per thread.
    #[test]
    fn a_released_cell_is_reused_and_re_armed() {
        let _g = registry_lock();
        let first = acquire_cell();
        let address = core::ptr::from_ref(first) as usize;
        // Simulate the thread's death and a completed rendezvous.
        first.alive.store(DEAD, Ordering::Relaxed);
        first.state.store(FINISHED, Ordering::Relaxed);
        release_cell(first);

        let second = acquire_cell();
        assert_eq!(
            core::ptr::from_ref(second) as usize,
            address,
            "the freed cell is handed straight back out"
        );
        assert_eq!(second.alive.load(Ordering::Relaxed), ALIVE);
        assert_eq!(second.state.load(Ordering::Relaxed), JOINABLE);
        assert!(second.outcome.load(Ordering::Relaxed).is_null());
    }

    /// A retired (detached) cell is **not** reused while its thread may still
    /// be running: the kernel still holds its word's address. It returns to
    /// circulation only once that word reads dead.
    #[test]
    fn a_retired_cell_is_recycled_only_after_the_kernel_records_the_death() {
        let _g = registry_lock();
        let cell = acquire_cell();
        let address = core::ptr::from_ref(cell) as usize;
        retire_cell(cell);

        // Still alive: a fresh spawn must not be handed this cell.
        let other = acquire_cell();
        assert_ne!(
            core::ptr::from_ref(other) as usize,
            address,
            "a live thread's cell must never be recycled — the kernel still \
             writes that word at its death"
        );

        // The kernel records the death; the next acquisition sweeps it back.
        cell.alive.store(DEAD, Ordering::Relaxed);
        let recycled = loop {
            let candidate = acquire_cell();
            if core::ptr::from_ref(candidate) as usize == address {
                break candidate;
            }
        };
        assert_eq!(recycled.alive.load(Ordering::Relaxed), ALIVE);
    }

    #[test]
    fn a_thread_id_carries_the_kernel_task_id_unchanged() {
        assert_eq!(ThreadId(0x1234).as_u64(), 0x1234);
        // Distinct ids never compare equal, so a stale handle cannot be
        // mistaken for a live thread.
        assert_ne!(ThreadId(1), ThreadId(2));
    }

    /// The payload's runner sits at offset zero, which is what lets the one
    /// type-erased entry recover the closure's type without knowing it.
    #[test]
    fn the_payload_head_is_the_runner() {
        let _g = registry_lock();
        let cell = acquire_cell();
        let payload = Box::new(Payload::<fn() -> u8, u8> {
            run: run::<fn() -> u8, u8>,
            cell,
            body: || 7u8,
            _outcome: PhantomData,
        });
        let base = core::ptr::from_ref(payload.as_ref()) as usize;
        let head = core::ptr::from_ref(&payload.run) as usize;
        assert_eq!(head, base, "the runner must be the payload's first field");
        // SAFETY: reading the head back out is exactly what the erased entry
        // does; comparing the function addresses proves it recovers the right
        // monomorphisation.
        let read: Runner = unsafe { core::ptr::read(base as *const Runner) };
        assert_eq!(
            read as *const () as usize,
            run::<fn() -> u8, u8> as *const () as usize
        );
    }

    /// On the host there is no trap, so a spawn fails closed — and it must
    /// leave neither the cell nor the payload behind.
    #[test]
    fn a_refused_spawn_returns_its_cell_and_frees_its_payload() {
        let _g = registry_lock();
        let before = {
            let registry = REGISTRY.lock();
            registry.free.len()
        };
        let refused = Thread::spawn(|| 0u8);
        assert!(
            refused.is_err(),
            "the host has no syscall trap, so a spawn must fail closed"
        );
        let after = {
            let registry = REGISTRY.lock();
            registry.free.len()
        };
        assert_eq!(
            after,
            before + 1,
            "the refused spawn's cell is returned for immediate reuse"
        );
    }
}
