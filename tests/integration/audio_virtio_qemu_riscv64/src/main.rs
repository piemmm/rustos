//! `plans/SOUND.md` SND4 QEMU integration test: boot the production
//! riscv64 `tairix-kernel` pipeline against the shared whole-disk audio-root
//! image — whose read-only `/System` volume carries the kernel-signed virtio
//! sound driver bundle and the test-only `audiotone` fixture — with a virtio
//! sound device attached behind QEMU's `wav` audio backend, and prove the
//! whole audio path end to end with an exact numeric assertion.
//!
//! ## What this vertical asserts
//!
//! The production boot path discovers the sound device, `devmgr` autoloads
//! the signed driver into its **own user process** (it claims a reserved
//! `audiochan-v1` endpoint and publishes its channel node), `devmgr` hands
//! that endpoint to **`audiod`**, and the scripted root shell runs
//! `audiotone`, which opens an `audio-v1` stream, queues a deterministic
//! signal, starts the device and drains. Three processes and two shared PCM
//! rings are between the program and the hardware, so a frame provably
//! crosses two real process boundaries here.
//!
//! The guest's own `AUDIO PASS` witness says the stack reported success with
//! no lost frames. The **host-side** assertion says more: QEMU's `wav`
//! backend wrote what the emulated card actually received, and the harness
//! checks it holds exactly the samples the guest played. A mixer that
//! silently substituted, resampled or dropped frames would still print the
//! witness, so the run needs both.
//!
//! The run is deterministic rather than a race: the ring is sized to hold the
//! whole signal, so every frame is queued before the device is clocked and
//! the device cannot run dry however slowly the emulated machine runs.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives; there is
//! no in-kernel QEMU-exit shortcut to leak into a production build.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{handle_panic_via_serial, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::riscv64::boot as boot_riscv64;

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the boot
    /// trampoline does not zero its bytes (the bump allocator does not require
    /// zeroed backing) and the boot pipeline excludes it from the usable
    /// physical-memory map, exactly as the production riscv64 kernel binary's
    /// heap does. `static mut` because the bump allocator hands out disjoint
    /// slices via an atomic cursor; the storage is otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Forward to the shared riscv64 panic bridge. A panic parks the hart; the
    /// guest never self-exits, so the run times out and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_audio_virtio_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with [`SERIAL_SINK`] taking both the log and the
    /// audit streams, so every boot/autoload/bind/echo record reaches the QEMU
    /// transcript for diagnosis. The guest does not self-exit: the harness ends
    /// the run when the host peer confirms the echo round-trip (its success
    /// gate), so teardown can never precede that confirmation. Boot at the
    /// default `Info` filter: keeping the noisier `Debug` syscall trace off the
    /// wire stops the NULL-console login read-retry chatter from crowding the
    /// network timeline out of a failing run's serial tail.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(
            hartid,
            dtb,
            &ALLOCATOR,
            &SERIAL_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
