//! Erasing secret bytes so the compiler cannot delete the erasure.
//!
//! A password, key, or capability token that has been used is still in
//! memory afterwards, and that memory outlives the value: a stack buffer is
//! reused by the next call frame, a heap block is handed to the next
//! allocation. Overwriting it is the only thing that ends its lifetime as a
//! secret.
//!
//! A plain `buf.fill(0)` does not do that. Nothing reads the buffer
//! afterwards, so the write is dead by the language's own rules and an
//! optimiser is entitled to remove it — which is exactly what a release
//! build does. [`wipe`] writes through [`write_volatile`](core::ptr::write_volatile)
//! instead, which the compiler must emit, and fences afterwards so the
//! stores are not sunk past the point the caller believes the secret is
//! gone.
//!
//! [`Wiped`] applies the same erasure to a fixed-size buffer at the end of
//! its scope, including on an early return or an unwind, so a caller cannot
//! grow a new exit path that forgets to erase.

use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

/// Overwrite every byte of `bytes` with zero, defeating dead-store
/// elimination.
///
/// Call this on any buffer that held a password, key, or capability token
/// before it goes out of scope or is reused. The write is volatile, so it
/// survives optimisation, and a fence after it stops the stores being
/// reordered past subsequent code.
///
/// This erases the bytes at the address given, and only those. A `String`
/// or `Vec` that reallocated while it held the secret left a copy in the
/// freed block that no later wipe can reach — size such a buffer once, up
/// front, so it never grows.
///
/// ```
/// let mut password = *b"correct horse";
/// tairix_util::secret::wipe(&mut password);
/// assert_eq!(password, [0u8; 13]);
/// ```
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a live, exclusively-borrowed, aligned `u8` for
        // the duration of the write, so writing a `u8` through it is
        // in-bounds and initialises what it overwrites. Volatility is what
        // is wanted here rather than what makes it sound: it forbids the
        // compiler from eliding a store nothing reads back.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// A fixed-size byte buffer that erases itself when it goes out of scope.
///
/// Use it wherever a secret is marshalled through a buffer: encoding a
/// password into a request, reading one out of a reply. Every exit from the
/// scope erases the bytes — the value returned, the `?` that returned early,
/// the panic that unwound — so no future edit can add a path that leaks the
/// contents by forgetting to clean up.
///
/// The buffer derefs to `[u8; N]`, so it is used exactly like the array it
/// wraps.
///
/// ```
/// use tairix_util::secret::Wiped;
///
/// let mut buf = Wiped::<8>::new();
/// buf[..6].copy_from_slice(b"secret");
/// assert_eq!(&buf[..6], b"secret");
/// // Dropping `buf` here overwrites all eight bytes.
/// ```
#[derive(Debug)]
pub struct Wiped<const N: usize>([u8; N]);

impl<const N: usize> Wiped<N> {
    /// A zeroed buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self([0; N])
    }

    /// Erase the buffer now, rather than waiting for the end of the scope.
    ///
    /// Dropping it erases it again; erasing twice costs one pass and is
    /// never wrong.
    pub fn wipe(&mut self) {
        wipe(&mut self.0);
    }
}

impl<const N: usize> Default for Wiped<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Deref for Wiped<N> {
    type Target = [u8; N];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> DerefMut for Wiped<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const N: usize> Drop for Wiped<N> {
    fn drop(&mut self) {
        self.wipe();
    }
}

#[cfg(test)]
mod tests {
    use super::{wipe, Wiped};

    #[test]
    fn wipe_zeroes_every_byte() {
        let mut buf = [0xAAu8; 64];
        wipe(&mut buf);
        assert_eq!(buf, [0u8; 64]);
    }

    #[test]
    fn wipe_of_an_empty_slice_is_harmless() {
        let mut empty: [u8; 0] = [];
        wipe(&mut empty);
    }

    #[test]
    fn wipe_touches_only_the_slice_it_was_given() {
        let mut buf = [0xFFu8; 8];
        wipe(&mut buf[2..5]);
        assert_eq!(buf, [0xFF, 0xFF, 0, 0, 0, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn a_wiped_buffer_starts_zeroed_and_reads_back_what_was_written() {
        let mut buf = Wiped::<16>::new();
        assert_eq!(*buf, [0u8; 16]);
        buf[..6].copy_from_slice(b"secret");
        assert_eq!(&buf[..6], b"secret");
        assert_eq!(buf.len(), 16);
    }

    #[test]
    fn wiping_early_clears_the_buffer_in_place() {
        let mut buf = Wiped::<16>::new();
        buf[..6].copy_from_slice(b"secret");
        buf.wipe();
        assert_eq!(*buf, [0u8; 16]);
    }

    /// Going out of scope must erase the bytes, not merely release them.
    #[test]
    fn dropping_a_wiped_buffer_erases_it() {
        let mut buf = core::mem::ManuallyDrop::new(Wiped::<16>::new());
        buf[..6].copy_from_slice(b"secret");
        let bytes = core::ptr::addr_of!(buf.0);

        // SAFETY: `buf` is a `ManuallyDrop`, so its destructor has not run
        // and its storage belongs to this frame for the whole test. Running
        // that destructor by hand leaves the storage in place — the buffer
        // owns nothing but plain bytes — so reading it back afterwards
        // observes exactly what the destructor wrote into it.
        unsafe {
            core::mem::ManuallyDrop::drop(&mut buf);
            assert_eq!(*bytes, [0u8; 16], "the destructor erased the secret");
        }
    }
}
