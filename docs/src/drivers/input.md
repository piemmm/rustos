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

| Driver        | Crate                            | Hardware                         | Status                          |
|---------------|----------------------------------|----------------------------------|---------------------------------|
| ps2           | `rustos-drv-input-ps2`           | Intel 8042 keyboard controller   | host-side tests + QEMU vertical |
| virtio_input  | `rustos-drv-input-virtio-input`  | virtio-input (keyboard / pointer) | host-side tests + QEMU vertical |

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

### `rustos-drv-input-virtio-input`

The virtio-input driver implements `Input` over the bus-agnostic virtio
transport from `lib/virtio`, so one source compiles against both the PCI
and MMIO transports (the queue protocol lives once, `AGENTS.md` §2.2).
It is the paravirtualised input device every QEMU machine type can
present (`virtio-keyboard-device` / `virtio-mouse-device` /
`virtio-tablet-device`) and the input class real virtio hardware
exposes — the `virt`-board analogue of the x86 PS/2 keyboard.

It consumes the device-to-driver **event queue** (queue 0). The wire
record is `struct virtio_input_event { __le16 type; __le16 code;
__le32 value; }` (virtio 1.1 §5.8.6) in the Linux `evdev` namespaces,
which `poll` maps onto the platform-neutral `InputEvent`: `EV_KEY` →
`Key` (the evdev keycode, `value` 1 press / 0 release), `EV_REL` `REL_X`
/ `REL_Y` → `Pointer`, and `REL_WHEEL` → `Scroll`. `EV_SYN` frame
separators and any unmodelled `type`/`code` are consumed but surface no
event, so the driver never fabricates a bogus one (`AGENTS.md` §2.9).

`open` runs the virtio 1.1 §3.1 init sequence (negotiating only
`VIRTIO_F_VERSION_1`, the modern split-virtqueue layout) and then
**pre-posts a pool of device-write event buffers**, keyed by the
descriptor head the queue assigns. A single posted buffer is not enough:
the device fills one buffer per event of a report, so a keypress's
`EV_KEY` *and* its trailing `EV_SYN` each need a free buffer at once.
`poll` drains every completed event (interrupt-driven through the host's
IRQ waiter — no busy-spin, `AGENTS.md` §2.1), decodes it, and hands each
buffer straight back so the pool stays full. The driver allocates every
device-visible buffer through the host `VirtioHost` DMA seam and reaches
the device only through the `Transport` seam, so it holds no ambient
authority (`AGENTS.md` §4 / §17.4).

QEMU integration on a live device is exercised by
`tests/integration/input_virtio_mmio_qemu_aarch64`
(`rustos-test-input-virtio-mmio-qemu-aarch64`, enrolled in `cargo xtask
test --qemu`). It boots the aarch64 `virt` board, builds the
virtio-MMIO transport from the embedded device tree, arms the GICv2 SPI
+ EL1 IRQ path, mints a `KernelVirtioHost`, loads this driver's signed
`.rxe` through `rustos_drvhost::Host`, and drives it through
load → use → unload → reload. "Use" is a **real injected key**: once the
guest logs its event-queue-armed readiness marker, the QEMU runner
(`tools/qemu`) attaches a `virtio-keyboard-device` and sends a key
through the QEMU monitor (`sendkey`); the eventq IRQ fires and the
driver decodes the press and, after reload, the matching release — the
virtio-input analogue of the PS/2 vertical's `0xD2` injection, with the
event originating device-side rather than guest-side.

The riscv64 `virt`-board sibling
`tests/integration/input_virtio_mmio_qemu_riscv64`
(`rustos-test-input-virtio-mmio-qemu-riscv64`, also enrolled in `cargo
xtask test --qemu`) drives the same driver and the same shared
`virtio_input_keypress` key-decode tail over the riscv64 MMIO bring-up
(PLIC source + S-mode trap path), so a single driver source covers the
`input` row of the QEMU matrix on x86_64 (PS/2), aarch64, and riscv64
(`AGENTS.md` §2.2).
