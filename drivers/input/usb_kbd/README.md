# `rustos-drv-input-usb-kbd` — USB HID boot-keyboard class driver

`plans/USB.md` U4. The autoloaded **user-space HID boot-keyboard *class*
driver** — the `Run` binary `devmgr` spawns when a HID boot-keyboard
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
3. Runs `rustos_hid::pump_once` over a `UrbReportSource`: each `next_report`
   submits a **blocking** interrupt-IN URB (`rustos_usb::UrbClient` over
   `ipc_call`) and copies the delivered report out of the shared buffer. The
   HCD leaves the call outstanding and replies only when the controller's
   completion interrupt delivers a report, so this driver **parks in the kernel
   between keystrokes** rather than busy-polling (`AGENTS.md` §2.23). If the
   HCD reports `NotFound`, the interface has vanished; the driver exits so
   `devmgr` can load a fresh instance when the HCD publishes the replugged
   interface.
4. Decodes each boot report through `rustos_hid` and injects each key edge into
   the kernel input-focus arbiter via `key_inject`.

A failure to acquire the host or the transport grants exits with a reserved
fail-closed code (`80` no host, `81` no transport), leaving the console without
a keyboard rather than wedged (`AGENTS.md` §2.9).

## Least privilege (`AGENTS.md` §5.4)

`CAP_INPUT_INJECT` (inject key edges), `CAP_SHM` (map the granted URB buffer),
`CAP_IPC_ENDPOINT` (submit URBs on its one interface's transport endpoint),
`CAP_LOG_EMIT` (one-shot beacon). A compromised keyboard driver cannot
reprogram the controller, reach another device's buffer, or touch the bus.

## Why a separate crate

The reusable HID boot-report **decode** lives in `lib/hid`; this crate is a
*separate* binary so it links the userland runtime `rustos-rt` and depends only
on `lib/*` crates (`lib/hid`, `lib/usb`, `lib/drvrt`, `lib/rt`, `lib/caps`,
`lib/abi`) — reaching its host-controller driver only through the public URB
transport ABI, so the §17.4 layering holds (no `drivers/*`→`drivers/*` edge).

## Supported hardware / limitations

Any HID **boot-protocol** keyboard interface a host-controller driver enumerates
and serves over the URB transport (the Pi 4's USB-A ports via the xHCI HCD).
Boot protocol only — no report-descriptor parse. The live report path is an
on-metal acceptance item; QEMU models no Pi USB timing (`AGENTS.md` §0.4).

## Tests

The decode logic is host-tested in `lib/hid`; the URB transport in `lib/usb`;
the grant accessors in `lib/drvrt`. This crate is the thin wiring binary (an
inert host stub off bare-metal targets, so `cargo build --workspace` / clippy /
fmt cover it) with unit coverage for its bounded pump-error policy and terminal
disconnected-transport classification. The end-to-end path is the metal
acceptance item (`plans/USB.md` U5).
