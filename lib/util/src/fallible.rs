//! Fallible allocation for buffers whose size comes from the data.
//!
//! Userland's heap answers exhaustion with a null pointer, which `alloc`
//! turns into `handle_alloc_error` — a process abort. A buffer sized by
//! image geometry or by a decoded payload is close to a megabyte at desktop
//! scale, so it is what a machine short of memory actually refuses, and a
//! program that has to degrade through that state cannot reach it through an
//! infallible growth. These reserve first and report the refusal, so it
//! reaches the caller that already publishes one.
//!
//! Two growth policies, because the buffers have two lifetimes: a one-shot
//! buffer reserves exactly what it needs, and a scratch that is grown across
//! uses reserves amortised so repeated growth is not quadratic.

use alloc::vec::Vec;

/// `count` copies of `value`, or `None` when the allocator refuses room for
/// them.
pub fn filled<T: Clone>(count: usize, value: T) -> Option<Vec<T>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(count).ok()?;
    buffer.resize(count, value);
    Some(buffer)
}

/// The first `count` of `items`, or `None` when the allocator refuses room
/// for them.
///
/// The take is what keeps the reservation exact: an iterator yielding more
/// than it claimed would otherwise grow the buffer past what was reserved.
pub fn collected<T>(count: usize, items: impl Iterator<Item = T>) -> Option<Vec<T>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(count).ok()?;
    buffer.extend(items.take(count));
    Some(buffer)
}

/// Lengthen a reused `scratch` to `count` copies, padding with `value`,
/// reporting whether it now holds them. A scratch already that long keeps
/// its capacity.
#[must_use]
pub fn grow_to<T: Clone>(scratch: &mut Vec<T>, count: usize, value: T) -> bool {
    let Some(extra) = count.checked_sub(scratch.len()) else {
        return true;
    };
    if scratch.try_reserve(extra).is_err() {
        return false;
    }
    scratch.resize(count, value);
    true
}

/// Room in `buffer` for `count` more items, reporting whether the allocator
/// granted it — for a buffer filled by pushing rather than by resizing.
#[must_use]
pub fn reserve<T>(buffer: &mut Vec<T>, count: usize) -> bool {
    buffer.try_reserve_exact(count).is_ok()
}

#[cfg(test)]
#[path = "fallible_tests.rs"]
mod tests;
