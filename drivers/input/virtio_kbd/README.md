# `tairix-drv-input-virtio-kbd`

The autoloaded **user-space virtio-input keyboard driver process** — the `Run`
binary `devmgr` spawns when a virtio-input device is discovered (`AGENTS.md`
§18, `plans/PI.md` P10 chunk 5d-2-ii). This is the "drivers in user space"
steady state (`AGENTS.md` §4) on the hardware QEMU `-M virt` actually presents;
the metal Pi 4 keyboard is the USB `drivers/input/usb_kbd`.

## What it does

`main` (in `src/main.rs`, a freestanding pure-Rust `tairix-rt` program):

1. Builds `tairix_drvrt::RtDriverHost::from_grants_query` over the
   device-resource grants the kernel minted for this process (coherency `None` —
   the kernel carves coherent DMA and the QEMU `virt` virtio interconnect snoops
   the CPU caches, so the binary stays platform-neutral, `AGENTS.md` §2.20).
2. Resolves its single granted register window with
   `tairix_abi::driver::sole_register_window` over `RtDriverHost::resources()` —
   the one definition of "which window did the kernel grant me", shared with the
   USB keyboard driver (`AGENTS.md` §2.2 / §2.16) — no build-time board constant.
3. Maps the window through the host's `mmio_map`, builds the bus-agnostic
   `tairix_virtio::MmioTransport` over it, and brings the device online with
   `tairix_virtio_input::VirtioInput::open` (the host is the `VirtioHost` the
   device carves its event buffers from).
4. Loops `VirtioInput::poll`, resolving each decoded `evdev` key edge into a
   `KeyInput` record with `tairix_virtio_input::VirtioKeyboardConsole` and
   injecting it into the kernel input-focus arbiter through the `key_inject`
   syscall, yielding between polls (`AGENTS.md` §2.1).

Every capability and bound is re-checked kernel-side (`AGENTS.md` §5.4); the
host adds no authority. A bring-up failure exits with a reserved fail-closed
code (`80` no host, `81` no register window, `82` bring-up failed), leaving the
console without a keyboard rather than wedged (`AGENTS.md` §2.9); the spawning
supervisor decides whether to relaunch.

## Why a separate crate

The reusable open/poll/decode device logic and the `evdev` console producer
live in `lib/virtio_input`, and the §8 driver identity (`register` + bind table)
in `drivers/input/virtio_input`. This crate is a *separate* binary so it can
link the userland runtime `tairix-rt` without pulling it into the kernel-linked
`virtio_input` driver, and it depends only on `lib/*` crates (`lib/virtio`,
`lib/virtio_input`, `lib/drvrt`, `lib/rt`, `lib/caps`, `lib/abi`) so the §17.4
layering holds (no `drivers/*`→`drivers/*` edge — the `lib/usb` ↔
`drivers/bus/usb` precedent).

## Supported hardware / limitations

Any virtio-input device (virtio 1.1 §5.8) whose register window and DMA
constraint the kernel granted this process. The keyboard `evdev`-keycode
resolver is the US QWERTY layout; pointer/scroll events are decoded by
`lib/virtio_input` but this binary injects only keyboard edges. The match key
carries no transport detail, so the same driver binds a virtio-input device
attached over PCI or MMIO (`AGENTS.md` §2.2).

## Capabilities

Granted at spawn from its matched node's requested resources: `CAP_MMIO_MAP`
(the register window), `CAP_MEM_DMA` (the event-buffer DMA region), and
`CAP_INPUT_INJECT` (`key_inject`).

## Tests

The decode/console logic is host-tested in `lib/virtio_input`; this crate is the
thin wiring binary (an inert host stub off bare-metal targets, so
`cargo build --workspace` / clippy / fmt cover it). The end-to-end autoload path
is the `-M virt` acceptance vertical (`plans/PI.md` P10 chunk 5d-2-ii).
