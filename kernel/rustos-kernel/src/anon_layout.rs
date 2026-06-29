//! Discovered-RAM sizing policy for a task's non-`FIXED` anonymous-heap
//! virtual window.
//!
//! The window's *base* and the other user-space layout offsets live in
//! [`crate::spawn_layout`]; its *size* is a function of the machine, not a
//! hand-wired constant, and lives here so the pure arithmetic is unit
//! tested on the CI host without dragging the freestanding-only layout
//! constants into the host build. Each port computes the page count at
//! spawn time from the frame allocator's discovered total and its own
//! user-VA ceiling (`USER_VA_TOP`), then hands it to
//! [`rustos_kernel_mem::LiveSpace`].

use rustos_kernel_mem::PAGE_SIZE;

/// Floor for the anonymous-heap window, in pages (16 MiB).
///
/// A machine with very little RAM still gets a usable heap window: the
/// window costs no RAM until the frame allocator backs a mapping (which
/// fails closed as a deterministic OOM), so a floor above tiny RAM is free
/// and over-commits nothing. Clamped down only if the address space above
/// the window base genuinely cannot hold even this (a degenerate
/// misconfiguration).
pub const ANON_WINDOW_MIN_PAGES: usize = 0x1000;

/// Pages of *address space* the non-`FIXED` anonymous-heap window spans for
/// a machine with `ram_frames` total frames, an anonymous-window base of
/// `anon_window_base`, and a per-architecture first-non-addressable user
/// virtual address `user_va_top`.
///
/// The capacity is **derived from discovered hardware**, never a hard-wired
/// `const` ceiling: a process may map anonymous memory up to the size of
/// physical RAM — the true upper bound, since no more anonymous pages can
/// ever be backed than there are frames — clamped to the virtual address
/// space actually available above the window base (so it can never run past
/// the per-arch user-VA top) and floored at [`ANON_WINDOW_MIN_PAGES`] so a
/// tiny machine still gets a workable heap. On a 1 GiB machine this yields
/// the same 1 GiB window the former fixed constant gave; on a large server
/// it scales with RAM instead of capping every process at 1 GiB, and on a
/// tiny board it reserves no more virtual span than the machine could back.
/// The window costs no RAM until a mapping is backed, and a request beyond
/// it fails closed (`OutOfMemory`) at the virtual reservation before any
/// frame is touched.
///
/// `user_va_top` is supplied by each port: the paging mode dictates it, so
/// it is genuinely target-specific and this neutral policy never names an
/// architecture.
#[must_use]
pub fn anon_window_pages(ram_frames: u64, anon_window_base: u64, user_va_top: u64) -> usize {
    let va_fit_pages = user_va_top.saturating_sub(anon_window_base) / PAGE_SIZE as u64;
    // Floor, but never beyond what the address space above the base can hold.
    let floor = (ANON_WINDOW_MIN_PAGES as u64).min(va_fit_pages);
    let pages = ram_frames.clamp(floor, va_fit_pages);
    // The bare-metal ports are 64-bit, so this never truncates; clamp to the
    // platform word width rather than ever panicking on the cast.
    usize::try_from(pages).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::{anon_window_pages, ANON_WINDOW_MIN_PAGES};
    use rustos_kernel_mem::PAGE_SIZE;

    // A representative per-arch user-VA ceiling (aarch64's 512 GiB TTBR0
    // window); the exact value only has to be far above the window base.
    const USER_VA_TOP: u64 = 1 << 39;
    // The window sits 4 GiB above a notional image bias of 0 (the topmost
    // user region — `spawn_layout::ANON_WINDOW_OFFSET`).
    const BASE: u64 = 0x1_0000_0000;

    #[test]
    fn scales_with_ram_on_a_large_machine() {
        // 64 GiB of RAM: the window tracks physical frames, not a 1 GiB cap.
        let ram_frames = 64 * 1024 * 1024 * 1024 / PAGE_SIZE as u64;
        assert_eq!(
            anon_window_pages(ram_frames, BASE, USER_VA_TOP) as u64,
            ram_frames
        );
    }

    #[test]
    fn one_gib_machine_matches_the_former_fixed_window() {
        // The former hand-wired constant was 0x4_0000 pages = 1 GiB; a 1 GiB
        // machine still yields exactly that, so nothing regressed for the
        // common case while larger machines now scale up.
        let ram_frames = 1024 * 1024 * 1024 / PAGE_SIZE as u64;
        assert_eq!(anon_window_pages(ram_frames, BASE, USER_VA_TOP), 0x4_0000);
    }

    #[test]
    fn tiny_machine_gets_the_floor() {
        // Far less RAM than the floor: the window costs no RAM until backed,
        // so a tiny board still gets a usable heap window (the floor), and a
        // map beyond real RAM still fails closed at frame exhaustion.
        let ram_frames = 100;
        assert_eq!(
            anon_window_pages(ram_frames, BASE, USER_VA_TOP),
            ANON_WINDOW_MIN_PAGES
        );
    }

    #[test]
    fn clamped_to_addressable_user_space() {
        // Enormous RAM but a deliberately tiny VA window above the base: the
        // window can never exceed what the address space can hold, so it can
        // never run past the per-arch user-VA top.
        let tiny_top = BASE + 256 * PAGE_SIZE as u64;
        let huge_ram = 1u64 << 40;
        assert_eq!(anon_window_pages(huge_ram, BASE, tiny_top), 256);
    }

    #[test]
    fn degenerate_base_at_or_above_ceiling_yields_zero() {
        // A base at (or past) the user-VA top leaves no room: the window is
        // empty, which `LiveSpace::new` then rejects fail-closed rather than
        // wrapping into a bogus span.
        assert_eq!(anon_window_pages(1 << 30, USER_VA_TOP, USER_VA_TOP), 0);
        assert_eq!(anon_window_pages(1 << 30, USER_VA_TOP + 1, USER_VA_TOP), 0);
    }
}
