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

| Driver | Crate                    | Hardware                         | Stage 4 status                 |
|--------|--------------------------|----------------------------------|--------------------------------|
| ps2    | `rustos-drv-input-ps2`   | Intel 8042 keyboard controller   | host-side tests + QEMU vertical |

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

QEMU integration on a live controller is exercised by
`tests/integration/ps2_input_qemu_x86_64` (`rustos-test-ps2-qemu-x86-64`,
enrolled in `cargo xtask test --qemu`). It boots the production kernel,
loads this driver's signed `.rxe` through `rustos_drvhost::Host`, and
drives it through load → use → unload → reload. The boot hand-off it
needed is the x86_64 8-bit port seam
`rustos_arch_x86_64::pio::X86PortIo8` — the byte-wide sibling of the PCI
bus driver's 32-bit `X86PortIo`, supplying the only in-tree
`PortIo8` implementation (`AGENTS.md` §17.2 / §17.4).

"Use" is **interrupt-driven**. The vertical binds the keyboard line
(ISA IRQ-1 → GSI 1) in the production `rustos_kernel_irq::IrqTable`,
sets the i8042 keyboard-interrupt config bit, masks the legacy 8259
PIC, and unmasks GSI 1 through the published `IoApicController`. It then
makes a keypress deterministic without physical hardware via the i8042
`0xD2` ("write keyboard output buffer") command — injecting a scancode
through the same `PortIo8` backend the driver reads through asserts the
real IRQ-1 line. After `sti` the test waits on `IrqTable::try_wait_step`
until the IO-APIC → LAPIC → IDT → dispatcher → `IrqTable::fire`
round-trip reports `WaitStep::Ready`, then drains the byte through the
driver's `poll`, confirming the decoded press and — after reload — the
matching release. The driver itself stays read-only and polled; the
interrupt only signals *when* a byte is waiting. This shares the
external-IRQ trap glue that `tests/integration/irq_qemu_x86_64`
validates against the PIT.
