//! Shared user-space layout constants for the PID 1 (`init`) spawn seam
//! and the runtime `spawn` producer.
//!
//! Every architecture port lays its first user program — and every child
//! the `spawn` syscall builds — out at the *same* offsets above the
//! build-time image bias, with the *same* stack/MMIO-window sizing and the
//! *same* canary seeds. Those values are equal across the ports by
//! definition (they describe one user-space layout, not a per-architecture
//! register layout), so they live here once rather than being copy-pasted
//! into each `init_spawn` / `spawn_producer` sibling (`AGENTS.md` §2.2).
//!
//! The image bias itself (`INIT_USER_BIAS` / `SHELL_USER_BIAS`) is *not*
//! here: it is baked per embedded program by `build.rs`, so it genuinely
//! belongs beside each consumer. The absolute bases are derived as
//! `bias + <offset>` at each call site.

/// Offset of the user stack base above the image bias (1 MiB into the high
/// user region). The `rustos-rt` runtime carves scratch space off the
/// stack, so the stack must comfortably exceed it.
pub const USER_STACK_OFFSET: u64 = 0x10_0000;

/// User stack pages (1.125 MiB): generous headroom over the runtime's
/// scratch span plus call frames.
pub const USER_STACK_PAGES: u64 = 288;

/// Offset of the startup-vector block above the image bias (3 MiB up, well
/// clear of the program image and the stack).
pub const USER_BLOCK_OFFSET: u64 = 0x30_0000;

/// Offset of the device-window virtual region above the image bias
/// (`plans/PI.md` 5d-0-ii (b′)): placed 1 GiB above the image bias — far
/// clear of the program image, stack, and startup block — and well below
/// the 64 GiB user/identity ceiling the spawn-window check guards.
///
/// Only the aarch64 port maps a user device window today (the x86_64 and
/// riscv64 spawn paths grant no `mmio_map` window yet), so this constant —
/// shared by that port's `init_spawn` and `spawn_producer` (`AGENTS.md`
/// §2.2) — is gated to `kernel_isa = "aarch64"` to stay live everywhere it
/// is compiled (`AGENTS.md` §2.3).
#[cfg(kernel_isa = "aarch64")]
pub const MMIO_WINDOW_OFFSET: u64 = 0x4000_0000;

/// Pages backing the device-window region (1 MiB): generous headroom over
/// the few device windows a driver task maps (`AGENTS.md` §24.1). Gated to
/// the aarch64 port for the same reason as [`MMIO_WINDOW_OFFSET`].
#[cfg(kernel_isa = "aarch64")]
pub const MMIO_WINDOW_PAGES: usize = 256;

/// Offset of the non-`FIXED` anonymous-heap virtual window above the image
/// bias (`plans/PI.md` 5d-0-ii (c)): placed 2 GiB above the bias — above the
/// device window (1 GiB up) and far clear of the program image, stack, and
/// startup block — and well below the 64 GiB user/identity ceiling the
/// spawn-window check guards. The `mem_map` placement allocator hands each
/// non-`FIXED` request a base out of `[bias + ANON_WINDOW_OFFSET, … +
/// ANON_WINDOW_PAGES·4 KiB)`. Gated to the aarch64 port (the only port that
/// retains a live space today) for the same reason as [`MMIO_WINDOW_OFFSET`].
#[cfg(kernel_isa = "aarch64")]
pub const ANON_WINDOW_OFFSET: u64 = 0x8000_0000;

/// Pages backing the anonymous-heap window (1 GiB of *address space*). The
/// window costs no RAM until the frame allocator backs a mapping (which
/// fails closed as a deterministic OOM, `AGENTS.md` §4 / §24.1), so it is
/// sized generously for a userland heap; the placement allocator's own
/// memory is bounded by the live-region count, not the page count. Gated to
/// the aarch64 port for the same reason as [`ANON_WINDOW_OFFSET`].
#[cfg(kernel_isa = "aarch64")]
pub const ANON_WINDOW_PAGES: usize = 0x4_0000;

/// Per-process stack-canary seed handed to PID 1 `init` (`AGENTS.md`
/// §19.2). Any value; the kernel RNG-seeded canary is a later stage.
pub const INIT_CANARY: u64 = 0x1117_A5ED_C0DE_0001;

/// Per-process stack-canary seed handed to a spawned child (`AGENTS.md`
/// §19.2). Any value; the kernel RNG-seeded canary is a later stage.
pub const CHILD_CANARY: u64 = 0x1117_A5ED_C0DE_5E55;
