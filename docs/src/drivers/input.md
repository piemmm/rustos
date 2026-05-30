# Input drivers

Input drivers report user-generated events — keyboard, pointer, and
scroll — to the focused session. They implement
[`rustos_abi::driver::input::Input`](../abi/driver_traits.md#input) and
run as user-space drivers; key repeat, modifier/lock state, keymap
translation, and routing to a session live above this trait in
`userland/gui/wm` and the session layer.

## Class trait

`Input` is a single method:

| Method   | Purpose                                   | Capability gate          |
|----------|-------------------------------------------|--------------------------|
| `poll`   | drain pending events into a caller buffer | `DriverHandle` ownership |

Each `InputEvent` carries a `kind` (`Key`, `Pointer`, or `Scroll`), a
`code` (keycode or axis index), and a `value` (press/release or signed
delta). Per `AGENTS.md` §2.9 the trait never panics: an empty buffer
maps to `DriverError::BufferTooSmall`.

## Shipped drivers

| Driver | Crate                    | Hardware                         | Stage 4 status        |
|--------|--------------------------|----------------------------------|------------------------|
| ps2    | `rustos-drv-input-ps2`   | Intel 8042 keyboard controller   | host-side tests only  |

### `rustos-drv-input-ps2`

The PS/2 keyboard driver reads the Intel 8042 keyboard controller that
every x86 PC and QEMU's default `q35`/`i440fx` machines expose: a
status/command register at I/O port `0x64` and a data register at
`0x60`. It is a keyboard driver; the auxiliary (mouse) port is left to
a future pointer driver.

The driver never issues an `inb`/`outb` itself. It reaches the two
ports through the host-supplied `PortIo8` 8-bit port seam, which the
x86_64 architecture port implements (`AGENTS.md` §17.2 / §17.4). It
therefore carries no architecture `cfg` and no ambient authority over
the I/O port space (`AGENTS.md` §4).

`poll` reads the status register and, while the output buffer holds a
keyboard byte, consumes and decodes a scancode-set-1 byte stream into
platform-neutral keycodes: the base make code (`1..=0x7F`) for
unprefixed keys and `0xE000 | make` for `E0`-extended keys, with
`value == 1` for a press and `0` for a release. An `E0` prefix that
arrives at the end of one drain is latched and paired with its code on
the next `poll`. The drain stops at an empty buffer, at an auxiliary
byte (which it does **not** consume), at a full caller buffer, or when
a per-call read budget is exhausted — so a stuck controller can never
make the driver spin (`AGENTS.md` §2.1).

Lifecycle: `register` clears `CAP_DRV_LOAD`; `Ps2Keyboard::new` binds
the driver to the controller ports without performing any I/O; dropping
the `Ps2Keyboard` releases the backend (unload); constructing a fresh
instance reloads. The driver issues the controller no commands, so
unload leaves no state to quiesce.

QEMU integration on a live controller depends on the kernel wiring a
`PortIo8` backend over the legacy I/O ports and routing the i8042 IRQ
(line 1) to the user-space driver — the same boot hand-off prerequisite
the framebuffer and block drivers' QEMU verticals waited on. Until that
plumbing lands the driver is covered by host-side mock-controller unit
tests.
