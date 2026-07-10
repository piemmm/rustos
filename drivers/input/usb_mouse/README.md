# `rustos-drv-input-usb-mouse` — USB HID boot-mouse class driver

`plans/USB.md` §1.2. The autoloaded **user-space HID boot-mouse *class*
driver** — the `Run` binary `devmgr` spawns when a HID boot-mouse
**interface** node is discovered (`AGENTS.md` §18). The crate is a `lib` (its
`BIND_KEYS` bind table, host-compilable for the image builder) **and** a `Run`
binary (the process).

It is a pure class driver: it touches **no** controller register, owns **no**
controller DMA, holds **no** IRQ line. The host-controller driver
(`drivers/bus/usb/xhci`) owns the controller and serves this interface's
transfers over the bus-agnostic URB transport. The same binary works behind any
host controller that speaks the URB transport (`AGENTS.md` §2.20 / §17.4).

## What it does

`main` (a freestanding pure-Rust `rustos-rt` program):

1. Builds `rustos_drvrt::RtDriverHost::from_grants_query` over its granted
   resources (the interface node's two transport grants — no MMIO/DMA).
2. Reads the URB transport endpoint id (`RtDriverHost::urb_endpoint`) and maps
   the shared URB data buffer the HCD forwarded (`RtDriverHost::map_shared` →
   `shm_map`).
3. Polls `rustos_hid::BootMouse` over a `UrbReportSource`: each `next_report`
   submits a **blocking** interrupt-IN URB (`rustos_usb::UrbClient` over
   `ipc_call`) and copies the delivered report out of the shared buffer. The
   HCD leaves the call outstanding and replies only when the controller's
   completion interrupt delivers a report, so this driver **parks in the
   kernel between movements** rather than busy-polling (`AGENTS.md` §2.23).
   The poll drains one decoded event at a time, so every event a report
   decoded is injected before the next report read can park. If the HCD
   reports `NotFound`, the interface has vanished; the driver exits so
   `devmgr` can load a fresh instance when the HCD publishes the replugged
   interface.
4. Decodes each boot report through `rustos_hid::BootMouse` (button edges
   diffed, X/Y deltas, wheel) and injects each pointer record into the kernel
   input-focus arbiter via `pointer_inject`, using the one shared device→seat
   mapping `PointerInput::from_device_event` — the same mapping the virtio
   pointer driver uses, so the two can never diverge. Wheel/scroll ticks are
   decoded but deliberately not injected: the pointer record vocabulary
   carries no scroll consumer yet, and fabricating one is forbidden.

A failure to acquire the host or the transport grants exits with a reserved
fail-closed code (`80` no host, `81` no transport), leaving the desktop
without a pointer rather than wedged (`AGENTS.md` §2.9).

## Least privilege (`AGENTS.md` §5.4)

`CAP_INPUT_INJECT` (inject pointer records), `CAP_SHM` (map the granted URB
buffer), `CAP_IPC_ENDPOINT` (submit URBs on its one interface's transport
endpoint), `CAP_LOG_EMIT` (one-shot beacon). A compromised mouse driver cannot
reprogram the controller, reach another device's buffer, or touch the bus.

## Why a separate crate

The reusable HID boot-report **decode** lives in `lib/hid`; this crate is a
*separate* binary so it links the userland runtime `rustos-rt` and depends only
on `lib/*` crates (`lib/hid`, `lib/usb`, `lib/drvrt`, `lib/rt`, `lib/caps`,
`lib/abi`) — reaching its host-controller driver only through the public URB
transport ABI, so the §17.4 layering holds (no `drivers/*`→`drivers/*` edge).
It is a sibling of `drivers/input/usb_kbd`, not a `cfg` of it: two class
drivers for two device classes is the deliberate modular shape.

## Supported hardware / limitations

Any HID **boot-protocol** mouse interface a host-controller driver enumerates
and serves over the URB transport (the Pi 4's USB-A ports via the xHCI HCD).
Boot protocol only — no report-descriptor parse, so buttons beyond
left/right/middle and high-resolution wheels surface only their boot-protocol
subset. The live report path is an on-metal acceptance item; QEMU models no Pi
USB (`plans/PI.md` §0.4).

## Tests

The decode logic is host-tested in `lib/hid` (`mouse` + shared tests); the URB
transport in `lib/usb` (including the keyboard-and-mouse-together bring-up
regressions); the grant accessors in `lib/drvrt`. This crate is the thin
wiring binary (an inert host stub off bare-metal targets, so `cargo build
--workspace` / clippy / fmt cover it) with unit coverage for its bounded
pump-error policy and terminal disconnected-transport classification. The
end-to-end path is the metal acceptance item (`plans/USB.md` §3).
