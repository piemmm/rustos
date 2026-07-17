# `tairix-drv-input-ps2` — i8042 PS/2 keyboard driver

Stage 4 deliverable. Implements `tairix_abi::driver::input::Input` for a
keyboard attached to the Intel 8042 keyboard controller — the legacy
"PS/2" controller every x86 PC and QEMU's default `q35`/`i440fx`
machines expose. The driver polls the controller and decodes a
scancode-set-1 byte stream into platform-neutral `InputEvent`s.

## Supported hardware

| Platform | Controller                   | Ports          | Stage 4 status              |
|----------|------------------------------|----------------|-----------------------------|
| x86_64   | Intel 8042 keyboard port     | `0x60`, `0x64` | mock-host + QEMU integration |

This is a **keyboard** driver. The controller's auxiliary (mouse) port
is out of scope: when `poll` sees a byte tagged as auxiliary it stops
draining and leaves the byte in place for a future pointer driver
rather than consuming and discarding it.

The driver issues the controller **no** commands and relies only on the
power-on default of scancode-set translation (which QEMU and PC
firmware enable), so it neither resets nor reconfigures the controller.

### Keycode encoding

Events are `InputEvent { kind: Key, code, value }`. `code` is the
scancode-set-1 make code (`1..=0x7F`) for unprefixed keys, or
`0xE000 | make` for `E0`-extended keys; `value` is `1` for a press and
`0` for a release. Keymap translation to characters, key repeat, and
modifier/lock state are higher-layer concerns.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `Input::poll` is gated by ownership of the `DriverHandle` returned
  from `register`; the `Input` trait declares no additional per-method
  capability.

Port access goes through the host-supplied `PortIo8` 8-bit port seam,
which the x86_64 architecture port implements — the driver never issues
an `inb`/`outb` itself and holds no ambient authority over the I/O port
space (`AGENTS.md` §4 / §17.2 / §17.4). The driver runs in user space;
it does **not** request `CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `Ps2Keyboard::new` binds the
driver to the controller ports without performing any I/O; dropping the
`Ps2Keyboard` releases the backend (the unload step). Reloading is
constructing a fresh instance over the same ports, which the
`unload_then_reload_decodes_again` test exercises. Because the driver
issues no controller commands, unload has no hardware state to quiesce.

## Test surface

`cargo test -p tairix-drv-input-ps2` exercises, against an in-process
mock `PortIo8` controller:

- `register` capability gate.
- Empty-buffer rejection (`BufferTooSmall`) and empty-queue `Ok(0)`.
- Press/release decode and `E0`-extended keycode decode.
- Extended-prefix latching across `poll` calls.
- Skipping detection-error / overrun markers (`0x00`, `0x80`).
- Stopping at an auxiliary byte without consuming it.
- Filling the caller buffer and resuming on the next `poll`.
- A per-call read budget that bounds a stuck controller (no spin).
- The driver never writing the controller.
- Unload → reload round-trip.

12/12 host-side tests pass.

The QEMU integration vertical `tests/integration/ps2_input_qemu_x86_64`
(`tairix-test-ps2-qemu-x86-64`, enrolled in `cargo xtask test --qemu`)
boots the production kernel, loads this driver's signed `.rxe` through
`tairix_drvhost::Host`, and drives it through load → use → unload →
reload. The boot hand-off it needed is the x86_64 8-bit port seam
`tairix_arch_x86_64::pio::X86PortIo8` (the byte-wide sibling of the PCI
bus driver's `X86PortIo`). "Use" is made deterministic without a
physical keypress by the i8042 `0xD2` ("write keyboard output buffer")
command: the test injects a scancode into the controller's output
buffer through the same `PortIo8` backend the driver reads through, then
confirms the driver decodes the resulting press and, after reload, the
matching release. Keyboard-IRQ (line 1) routing is not required because
the driver polls; an interrupt-driven path remains a later follow-up.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`Ps2Keyboard` type and its `new` constructor are re-exported so the
driver host can construct an instance; the host reaches it only through
the `Input` trait afterwards.
