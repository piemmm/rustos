//! x86_64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9 Stage B).
//!
//! The Arch HAL [`MachineTakeover`] body for the x86_64 PC-class port: the
//! irreversible, one-way sequence the pre-boot Supervisor's `memtest full`
//! drives to test **all** of RAM. It is deliberately the only public surface
//! of this module — the takeover `static` is private and reachable solely
//! through the supervisor-gated [`machine_takeover_handle`], which the
//! downstream boot wrapper calls from its `KernelArch::machine_takeover`
//! accessor.
//!
//! # The x86_64 sequence
//!
//! [`X86MachineTakeover::take_over`] performs, in order and without ever
//! returning on success:
//!
//! 1. **Confirm this is the only running CPU.** The production x86_64 image
//!    starts no application processors, so there is nothing to quiesce; the
//!    step still *verifies* it and fails closed
//!    ([`TakeoverError::CpuQuiesceTimeout`]) rather than assuming it — a
//!    non-zero [`crate::smp::secondary_entry_addr`] means AP bring-up was
//!    wired without teaching this takeover to cooperatively stop the APs.
//! 2. **Mask interrupts** (`cli`) so nothing preempts the solitary CPU. The
//!    x86_64 port wires no lockup watchdog, so there is none to stop.
//! 3. **Switch onto a reserved stack** the sweep will not overwrite. It lives
//!    in the kernel image's `.bss` (reserved), which every address space maps
//!    in the higher half, so the switch is safe under whatever `%cr3` was
//!    active when the Supervisor was entered.
//! 4. **Install the reserved boot page tables** (`%cr3 = boot_pml4`). Unlike
//!    riscv64/aarch64, long mode cannot drop paging, so instead of flattening
//!    the MMU the takeover switches to the boot page tables — which live
//!    entirely in `.boot.bss` (reserved) and map both the higher-half kernel
//!    window (through which the sweep reaches physical RAM) and the low
//!    identity window — so the sweep never depends on a page-table frame in
//!    the *usable* RAM it is about to destroy.
//! 5. **Run the sweep** (the arch-neutral destructive test of every *usable*
//!    frame, which renders progress to the console) on the reserved stack.
//! 6. **Test the region the sweep could not** — the kernel image and the
//!    stack it ran on, the physical range `[__boot_phys_start,
//!    __kernel_phys_end)` — with the relocatable, register-only
//!    `_takeover_stub` copied into a just-swept usable page above the kernel
//!    image (the "arena"). Because long mode requires paging and the boot
//!    page tables sit inside the region under test, the takeover first builds
//!    a minimal identity page table in that same arena and hands the stub its
//!    `%cr3`; the stub installs it, tests the region, and resets the platform
//!    through the legacy 8042 / `0xCF9` reset hardware. The arena (the stub
//!    and its page tables) and the low firmware/ACPI reserved RAM are the only
//!    RAM excluded, both unavoidable.
//!
//! On any pre-destructive refusal `take_over` returns the [`TakeoverError`]
//! with the machine untouched and `sweep` un-run (fail closed, never a panic).

use tairix_arch_api::{MachineTakeover, TakeoverError};

use crate::paging::{flags, KERNEL_VMA_BASE, PAGE_SIZE};

extern "C" {
    /// First byte of the kernel image, at its 1:1 low *physical* load address
    /// (`linker.ld`: `. = 1M; __boot_phys_start = .`). The inclusive lower
    /// bound of the region the relocated stub self-tests; everything below it
    /// is low firmware / BIOS / ACPI reserved RAM the stub must never touch.
    static __boot_phys_start: u8;
    /// One-past-the-end *physical* address of the whole kernel image (boot
    /// trampoline through the end of `.bss`, including the bump heap;
    /// `linker.ld`: `__kernel_phys_end = . - KERNEL_VMA_BASE`). The exclusive
    /// upper bound of the stub's self-test region; 4 KiB-aligned by the
    /// linker.
    static __kernel_phys_end: u8;
    /// The boot PML4 (`boot.s`, `.boot.bss`), linked 1:1 in low memory so its
    /// symbol address **is** its physical address — the reserved `%cr3` the
    /// sweep runs under.
    static boot_pml4: u8;
}

/// Size of the reserved takeover stack, in bytes (64 KiB — matching the
/// riscv64/aarch64 ports and the boot stack's headroom). It lives in the
/// kernel image's `.bss` (reserved), so the sweep (which tests only *usable*
/// frames) never overwrites it; the relocated stub tests it as part of the
/// kernel-image region *after* the sweep has finished using it.
const TAKEOVER_STACK_BYTES: usize = 64 * 1024;

/// The reserved takeover stack the sweep runs on.
///
/// `UnsafeCell` because the CPU writes through it via `%rsp` while the Rust
/// aliasing model would otherwise treat a plain `static` as immutable; it is
/// never read as a Rust value, only used as raw stack backing. A takeover
/// happens at most once per boot (the machine resets), so there is no
/// concurrent access.
#[repr(C, align(16))]
struct TakeoverStack {
    bytes: core::cell::UnsafeCell<[u8; TAKEOVER_STACK_BYTES]>,
}

// SAFETY: the stack is only ever used as raw `%rsp` backing on the single CPU
// the takeover has quiesced every other CPU away from; there is no concurrent
// access and it is never read as a typed value.
unsafe impl Sync for TakeoverStack {}

/// The one reserved takeover stack. `.bss`-resident (zeroed), inside the
/// kernel image, so it survives the usable-RAM sweep.
static TAKEOVER_STACK: TakeoverStack = TakeoverStack {
    bytes: core::cell::UnsafeCell::new([0u8; TAKEOVER_STACK_BYTES]),
};

/// Round `addr` up to the next multiple of `align` (a power of two).
///
/// Operates in `u64` because x86_64 physical addresses are 64-bit (the arena
/// sits just above the kernel image, potentially anywhere in the low physical
/// window), so there is no lossy narrowing on the destructive path.
const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// The x86_64 machine-takeover handle (`plans/NEW-SUPERVISOR.md` §9).
///
/// A zero-sized unit: the takeover needs no per-instance state (the reserved
/// stack, the kernel-image bounds, the boot page tables, and the relocatable
/// stub are all `static`/linker-provided). Held by the downstream
/// `KernelArch` wrapper behind the supervisor-gated accessor and never
/// constructed elsewhere.
pub struct X86MachineTakeover;

/// The single `'static` takeover handle. Private to the crate: the only way
/// to reach it is [`machine_takeover_handle`], which the downstream boot
/// wrapper calls from its supervisor-gated `KernelArch::machine_takeover`.
static X86_TAKEOVER: X86MachineTakeover = X86MachineTakeover;

/// Hand back the `'static` x86_64 machine-takeover handle.
///
/// The downstream boot wrapper's `KernelArch::machine_takeover` — itself gated
/// on the supervisor-only `TakeoverGrant` — is the only caller, so the
/// destructive mechanism stays reachable exclusively from the confirmed
/// `memtest full` path.
#[must_use]
pub fn machine_takeover_handle() -> &'static (dyn MachineTakeover + Sync) {
    &X86_TAKEOVER
}

extern "C" {
    /// The relocatable, register-only kernel-image self-test + reset stub
    /// (`takeover.s`). Its address and [`_takeover_stub_end`] bound the bytes
    /// copied into the arena.
    fn _takeover_stub();
    /// One past the last byte of [`_takeover_stub`].
    fn _takeover_stub_end();
    /// Install the reserved stack and tail-call
    /// [`tairix_arch_x86_64_takeover_continue`] (`takeover.s`). Never returns.
    fn _takeover_switch_stack(thin: usize, stack_top: usize) -> !;
}

impl MachineTakeover for X86MachineTakeover {
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError {
        // 1. Confirm this is the only running CPU. The production x86_64
        //    image starts no APs, so `secondary_entry_addr()` is 0 and there
        //    is nothing to quiesce. A non-zero entry means AP bring-up was
        //    wired without teaching this takeover to cooperatively stop the
        //    APs, so it refuses fail-closed rather than destroy RAM another
        //    CPU is still using. (Logical CPU 1 is the first AP; the exact id
        //    is cosmetic on a path that cannot occur in the shipped image.)
        if crate::smp::secondary_entry_addr() != 0 {
            return TakeoverError::CpuQuiesceTimeout { cpu: 1 };
        }

        // 2. Mask interrupts so nothing preempts the solitary CPU. There is
        //    no lockup watchdog wired on this port, so there is none to stop.
        // SAFETY: `cli` clears only `RFLAGS.IF`; this is the deliberate,
        // confirmed tear-down the caller's `TakeoverGrant` authorises.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }

        // 3. Switch onto the reserved stack and run the sweep, then test the
        //    kernel-image region and reset — none of which returns. The sweep
        //    handle is reached through a thin pointer to the caller's `&mut
        //    dyn FnMut()`, read once at entry (on the still-live caller stack)
        //    before the switch. The reserved stack is `.bss`, mapped in the
        //    higher half under whatever `%cr3` is active, so the switch is
        //    valid before the boot page tables are installed (step 4, in the
        //    continuation).
        let mut sweep_ref: &mut dyn FnMut() = sweep;
        let thin = core::ptr::addr_of_mut!(sweep_ref) as usize;
        let stack_top = core::ptr::addr_of!(TAKEOVER_STACK) as usize + TAKEOVER_STACK_BYTES;
        // SAFETY: `_takeover_switch_stack` installs the 16-byte-aligned top of
        // the reserved `.bss` takeover stack and tail-calls
        // `tairix_arch_x86_64_takeover_continue(thin)`; `thin` addresses a
        // live `&mut dyn FnMut()` in reserved memory. It never returns.
        unsafe { _takeover_switch_stack(thin, stack_top) }
    }
}

/// Install the reserved boot page tables, run the destructive sweep on the
/// reserved stack, then test the region it executed from and reset. Entered
/// from `_takeover_switch_stack` with `%rsp` already on the reserved takeover
/// stack; never returns.
///
/// # Safety
///
/// Reached only from [`X86MachineTakeover::take_over`] after interrupts are
/// masked. `thin` is a live pointer to the caller's `&mut dyn FnMut()` sweep
/// handle, whose environment resides in reserved memory the sweep does not
/// destroy.
#[no_mangle]
unsafe extern "C" fn tairix_arch_x86_64_takeover_continue(thin: usize) -> ! {
    // 4. Install the reserved boot page tables (`%cr3 = boot_pml4`). They live
    //    in `.boot.bss` (reserved) and map both the higher-half kernel window
    //    the sweep writes physical RAM through and the low identity window the
    //    relocated stub later executes from, so nothing the sweep destroys is
    //    depended upon. `boot_pml4`'s symbol address is its physical address
    //    (linked 1:1 in low memory).
    let boot_cr3 = core::ptr::addr_of!(boot_pml4) as u64;
    // SAFETY: `boot_pml4` is the boot page-table root the kernel itself booted
    // on; loading it re-establishes the reserved mapping. Its higher-half
    // window maps this continuation's code and the reserved stack, so
    // execution and the stack survive the load; `mov`-to-`%cr3` flushes the
    // non-global TLB.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) boot_cr3, options(nostack, preserves_flags));
    }

    // 5. Run the destructive sweep over every usable frame.
    // SAFETY: `thin` points at the live `&mut dyn FnMut()` the caller placed
    // on its stack; reconstructing and calling it runs the architecture-neutral
    // destructive sweep, which reads/writes only reserved state and the
    // physical RAM it is meant to destroy.
    let sweep = unsafe { &mut *(thin as *mut &mut dyn FnMut()) };
    sweep();

    // 6. The sweep tested every *usable* frame. The one region it could not
    //    touch is the memory it ran from — the kernel image and this stack.
    // SAFETY: the sweep has completed; relocating the register-only stub into
    // a swept usable arena and jumping to it tests that region and resets,
    // never returning.
    unsafe { relocate_stub_and_reset() }
}

/// Build a minimal identity page table plus a copy of the relocatable stub in
/// a swept usable arena above the kernel image, then jump to the stub to test
/// `[__boot_phys_start, __kernel_phys_end)` and reset. Never returns.
///
/// # Safety
///
/// Called only from [`tairix_arch_x86_64_takeover_continue`] after the
/// usable-RAM sweep, with interrupts masked and the boot page tables active.
/// The arena is swept usable RAM above the kernel image; it is overwritten
/// wholesale, so it must not hold anything still needed.
unsafe fn relocate_stub_and_reset() -> ! {
    let start = core::ptr::addr_of!(__boot_phys_start) as u64;
    let end = core::ptr::addr_of!(__kernel_phys_end) as u64;

    // The arena: four contiguous 4 KiB pages of swept usable RAM immediately
    // above the kernel image — a PML4, a PDPT, one PD (identity-mapping the
    // low 1 GiB with 2 MiB huge pages, enough to cover the kernel image and
    // the stub's own execution page), and the stub's code page. Their
    // physical addresses are all below the 1 GiB higher-half window, so the
    // boot page tables let this code write them through
    // `KERNEL_VMA_BASE + phys`.
    let page = PAGE_SIZE as u64;
    let arena = align_up(end, page);
    let pml4_phys = arena;
    let pdpt_phys = arena + page;
    let pd_phys = arena + 2 * page;
    let stub_phys = arena + 3 * page;

    // A power-of-two "present + writable" leaf/table flag set and the 2 MiB
    // huge-page identity mapping, reusing the port's own PTE flag definitions
    // rather than re-spelling raw bits.
    let table_flags = flags::PRESENT | flags::WRITABLE;
    // SAFETY: each arena page is plain RAM below the 1 GiB higher-half window,
    // reachable at `KERNEL_VMA_BASE + phys` under the boot page tables just
    // installed, and outside the `[start, end)` region the stub will test, so
    // writing the page tables and stub bytes here corrupts nothing the stub
    // depends on. The pages were swept, so their prior contents are dead.
    unsafe {
        let pml4 = (KERNEL_VMA_BASE + pml4_phys) as *mut u64;
        let pdpt = (KERNEL_VMA_BASE + pdpt_phys) as *mut u64;
        let pd = (KERNEL_VMA_BASE + pd_phys) as *mut u64;
        // Zero the three table pages, then chain PML4[0] -> PDPT[0] -> PD.
        for i in 0..512 {
            pml4.add(i).write_volatile(0);
            pdpt.add(i).write_volatile(0);
        }
        pml4.write_volatile(pdpt_phys | table_flags);
        pdpt.write_volatile(pd_phys | table_flags);
        // Identity-map [0, 1 GiB) with 512 * 2 MiB huge pages (P|RW|PS).
        const HUGE_2MIB: u64 = 2 * 1024 * 1024;
        for k in 0..512u64 {
            pd.add(k as usize)
                .write_volatile(k * HUGE_2MIB | table_flags | flags::HUGE);
        }

        // Copy the relocatable stub into its arena page (via the higher-half
        // window). It will execute at its identity address `stub_phys`.
        let stub_src = _takeover_stub as *const () as *const u8;
        let stub_end = _takeover_stub_end as *const () as usize;
        let stub_len = stub_end - (_takeover_stub as *const () as usize);
        let stub_dst = (KERNEL_VMA_BASE + stub_phys) as *mut u8;
        core::ptr::copy_nonoverlapping(stub_src, stub_dst, stub_len);
    }

    // Jump to the stub at its low identity address (mapped executable by the
    // boot page tables' 0..4 GiB identity window). The stub installs the arena
    // `%cr3`, tests `[start, end)`, and resets — it never returns.
    // SAFETY: `stub_phys` holds the freshly-copied register-only stub, mapped
    // identity and executable under the active boot page tables; the arguments
    // are passed in the System V registers the stub's entry contract names,
    // and `dst` is a distinct scratch register so it cannot alias them.
    unsafe {
        core::arch::asm!(
            "jmp {dst}",
            in("rdi") start,
            in("rsi") end,
            in("rdx") pml4_phys,
            dst = in(reg) stub_phys,
            options(noreturn, nostack),
        );
    }
}
