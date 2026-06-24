# `rustos-drv-input-usb-kbd`

The autoloaded **user-space USB boot-keyboard driver process** — the `Run`
binary `devmgr` spawns when a HID boot-keyboard interface is discovered behind a
USB host (`AGENTS.md` §18, `plans/PI.md` P10 chunk 5d-2-ii). This is the
"drivers in user space" steady state (`AGENTS.md` §4).

## What it does

`main` (in `src/main.rs`, a freestanding pure-Rust `rustos-rt` program):

1. Builds `rustos_drvrt::RtDriverHost::from_grants_query` over the
   device-resource grants the kernel minted for this process (coherency `None` —
   the kernel carves coherent DMA, so the binary stays platform-neutral,
   `AGENTS.md` §2.20).
2. Derives its xHCI register BAR window + DMA aperture bound from the same
   delivered grants with `rustos_hid::derive_keyboard_resources` over
   `RtDriverHost::resources()` — no second `resource_grants` syscall, no
   build-time board constant (`AGENTS.md` §2.16 / §2.20).
3. Runs `rustos_hid::bring_up_boot_keyboard_diagnostic` to carve DMA, map the
   BAR, bring the controller up, and enumerate the boot keyboard.
4. Loops `rustos_hid::pump_once`, injecting each decoded key edge into the
   kernel input-focus arbiter through the `key_inject` syscall and yielding
   between polls (`AGENTS.md` §2.1).

Every capability and bound is re-checked kernel-side (`AGENTS.md` §5.4); the
host adds no authority. A bring-up failure emits a **one-shot structured
diagnostic** (`log_emit`, event `4126`) naming the phase that stalled and the
controller state observed there — the reset sub-stage + `USBCMD`/`USBSTS` for a
controller-open stall, or the `stage`/`completion`/`reject`/`evtype`
breadcrumbs + root-port `PORTSC` for an enumeration stall (`AGENTS.md` §15.7) —
then exits with a reserved fail-closed code (`80` no host, `81` no resources,
`82` bring-up failed), leaving the console without a keyboard rather than
wedged (`AGENTS.md` §2.9); the spawning supervisor decides whether to
relaunch. QEMU models no Pi USB, so this diagnostic is how an on-metal capture
localises the stall (`AGENTS.md` §0.4) — it is the user-space replacement for
the deleted in-kernel scaffold's per-stage logging.

## Why a separate crate

The reusable HID decode + orchestration lives in `lib/hid` and the §8 driver
identity (`register` + bind table) in `drivers/input/usb_hid`. This crate is a
*separate* binary so it can link the userland runtime `rustos-rt` without
pulling it into the kernel-linked `usb_hid` driver, and it depends only on
`lib/*` crates (`lib/hid`, `lib/drvrt`, `lib/rt`, `lib/caps`, `lib/abi`) so the
§17.4 layering holds (no `drivers/*`→`drivers/*` edge).

## Supported hardware / limitations

Any HID **boot-protocol** keyboard reachable through an xHCI controller whose
register BAR and DMA constraint the kernel granted this process (the Pi 4's
VL805, discovered and enumerated by `drivers/bus/pcie_brcm` +
`drivers/bus/usb`). Boot protocol only — no report-descriptor parse. The live
controller bring-up and report pump are an on-metal acceptance item; QEMU
models no Pi USB timing (`AGENTS.md` §0.4).

## Capabilities

Granted at spawn from its matched node's requested resources: `CAP_MMIO_MAP`
(the register BAR), `CAP_MEM_DMA` (the DMA region), `CAP_INPUT_INJECT`
(`key_inject`), and `CAP_LOG_EMIT` (the one-shot bring-up diagnostic, §19.4).

## Tests

The decode/orchestration logic is host-tested in `lib/hid`; this crate is the
thin wiring binary (an inert host stub off bare-metal targets, so
`cargo build --workspace` / clippy / fmt cover it). The end-to-end path is the
metal acceptance item.
