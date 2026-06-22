# `rustos-drv-bus-mailbox-vcmailbox` — VideoCore property-mailbox service driver

Autoloaded **user-space service driver** for the Broadcom BCM2711 (Raspberry
Pi 4) VideoCore firmware property mailbox. It owns the discovered mailbox
doorbell window and a DMA-carved property buffer and answers *synchronous*
property-channel exchanges from other user-space drivers over the well-known
`rustos_abi::mailbox_ipc::MAILBOX_ENDPOINT` call endpoint (`plans/PI.md` P10
D3).

This is the "drivers in user space" steady state (`AGENTS.md` §4): the
VideoCore mailbox is no longer a kernel facility. The §18.6 bootstrap floor
stays storage-only; this service is discovered and spawned by `devmgr` like
any other driver.

## Supported hardware

- BCM2711 VideoCore property mailbox (Raspberry Pi 4 / Pi 400). Binds by the
  device-tree `compatible` string `brcm,bcm2835-mbox` (the BCM2711 reuses the
  bcm2835 mailbox programming model).

The doorbell register base and the DMA property buffer are **discovered**
values threaded from the matched hardware-tree node's resource grants
(`AGENTS.md` §2.20 / §18.3); the driver names no board address.

## Protocol

The service is the server half of `rustos_abi::mailbox_ipc`: it `call_recv`s a
32-word `VideoCore` property buffer, runs the exchange over
`lib/vcmailbox::MmioMailbox`, and `call_reply`s a status-framed response. A
malformed request or a transport fault is answered as a fail-closed in-band
error reply, never a dropped caller (`AGENTS.md` §5.4 / §2.9). The
board-neutral protocol logic lives once in `lib/abi::mailbox_ipc::serve_request`
(`AGENTS.md` §2.2).

## Required capabilities

- `CAP_MMIO_MAP` — map the discovered doorbell window.
- `CAP_MEM_DMA` — carve the DMA-visible property buffer.
- `CAP_IPC_BIND_PRIVILEGED` — create the restricted-sender call endpoint
  (callers must hold `CAP_MAILBOX`).

It runs in user space (no `CAP_DRV_KERNEL`).

## Install

`cargo xtask image --target aarch64-rpi` cross-compiles this crate
position-independent for `aarch64-unknown-none` (its own `Run.ld`), converts
the linked PIE ELF to an `rxe`, and wraps it as a signed `kind = UserSpace`
`DriverManifest` (the capabilities above, the crate's `lib/vcmailbox::BIND_KEYS`
bind table, signed with the kernel's driver-signing seed). The bundle is
planted into the image's read-only `/System/Drivers/` store at
`bus_mailbox/vcmailbox/Run`, where the booted kernel's signed §18.6 load gate
admits it and `devmgr` autoloads it against the discovered mailbox node. See
`docs/src/platform/aarch64.md` ("VideoCore mailbox service").

## Limitations

- Single-board: BCM2711 only. Other VideoCore generations (Pi 3 BCM2837, Pi 5
  BCM2712) are out of scope here and reuse this work as a later board port.
- Verifiable on metal only: QEMU `virt` models no VideoCore mailbox
  (`plans/PI.md` §0.4), so there is no QEMU integration vertical; the wire
  protocol, the client channel, and the server transform are host-tested in
  `lib/abi`, `lib/drvrt`, and `lib/vcmailbox`.

## Stability

`experimental` — part of the in-progress P10 user-space driver migration.
