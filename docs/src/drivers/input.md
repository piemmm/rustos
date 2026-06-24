# Input drivers

Input drivers report user-generated events — keyboard, pointer, and
scroll — to the focused session. They implement
[`rustos_abi::driver::input::Input`](../abi/driver_traits.md#input) and
run as user-space drivers. For the **desktop** path, key repeat,
modifier/lock state, keymap translation, and routing to a surface live
above this trait in `userland/gui/wm` and the session layer. For a
**text console**, a keyboard driver instead translates its key events
to console bytes itself and injects them through the `console_input`
syscall (see "Console-input producer" below); key repeat remains a
higher-layer concern either way.

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
| usb_hid       | `rustos-drv-input-usb-hid`       | USB-HID boot keyboard / mouse    | host-side tests (P10; xHCI report path host-proven, PCI BAR wiring pending) |
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

`rustos-drv-input-virtio-input` is the §8 driver identity — the `register`
entry and the §18.3 `BIND_KEYS` bind table. The reusable open/poll/decode
device logic and the keyboard console producer described below live in the
[`rustos-virtio-input`](../lib/virtio_input.md) library (`lib/virtio_input`),
shared by the in-kernel `-M virt` verticals and the user-space input-driver
process (`rustos-drv-input-virtio-kbd`, below) without a
`drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4 / §2.2 — the virtio analogue of
`lib/hid` ↔ `drivers/input/usb_hid`).

The virtio-input logic implements `Input` over the bus-agnostic virtio
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

It publishes a canonical `BIND_KEYS` table (`AGENTS.md` §18.3): one
entry matching a probed virtio node whose device id is `virtio-input`
(`HwMatchKey::virtio(18)`) at the exact-match priority tier, the single
source of truth its signed-manifest bind table is authored from and the
data `devmgr` (or the in-kernel bootstrap-floor catalogue) resolves a
discovered virtio-input node against. The key carries no transport
detail, so the same driver binds whether the device is attached over
virtio-MMIO or PCI (`AGENTS.md` §2.2 / §17.4).

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

#### Console-input producer

`lib/virtio_input`'s `VirtioKeyboardConsole` is the keyboard producer half: it
turns the `Key` `InputEvent` edges `poll` decodes (whose `code` is the raw
`evdev` keycode) into the `rustos_abi::input::KeyInput` records a driver injects
through the `key_inject` syscall. It tracks the held modifiers (each of the
eight modifier keys independently, collapsing the left/right pairs) and the
caps-/num-lock toggles, resolves each printable or named key edge into the
`Key` a US keyboard layout produces, and builds the record through the shared
`rustos_keymap::key_input` map — the **one** `Key`→record definition the
`lib/hid` USB console producer reaches too (`AGENTS.md` §2.2). The
`evdev`-keycode→`Key` table is `evdev`-specific (a USB HID keyboard decodes HID
usages into the same `Key` vocabulary), so it lives here; everything is
allocation-free and fail-closed — an unknown keycode or a non-key event
produces no record rather than guessing (`AGENTS.md` §2.9).

#### The autoloaded driver binary (`rustos-drv-input-virtio-kbd`)

`drivers/input/virtio_kbd` (`rustos-drv-input-virtio-kbd`, `src/main.rs`) is the
autoloaded **user-space** virtio-input keyboard driver process — the "drivers in
user space" steady state (`AGENTS.md` §4) on the hardware QEMU `-M virt`
presents (the metal Pi 4 keyboard is the USB `rustos-drv-input-usb-kbd`). It is
a freestanding pure-Rust `rustos-rt` program depending only on `lib/*`
(`lib/virtio`, `lib/virtio_input`, `lib/drvrt`, `lib/rt`, `lib/caps`, `lib/abi`)
so the §17.4 layering holds. `main` builds `RtDriverHost::from_grants_query`
over its kernel-issued grants (coherency `None` — coherent DMA, platform-neutral
§2.20), resolves its single granted register window with
`rustos_abi::driver::sole_register_window` over `RtDriverHost::resources()` (the
one definition shared with the USB keyboard driver, §2.2 / §2.16), maps it
through `mmio_map`, builds the bus-agnostic `MmioTransport`, brings the device up
with `VirtioInput::open`, and loops `poll` → `VirtioKeyboardConsole::feed` →
`key_inject`, yielding between polls (`AGENTS.md` §2.1). Every capability and
bound is re-checked kernel-side (`AGENTS.md` §5.4); a bring-up failure exits with
a reserved fail-closed code (`80`/`81`/`82`). It is a separate crate from the §8
`rustos-drv-input-virtio-input` identity so it can link `rustos-rt` without
pulling it into the kernel-linked driver shell (`AGENTS.md` §2.2 — the `usb_kbd`
analogue).

### `rustos-drv-input-usb-hid`

`rustos-drv-input-usb-hid` is the §8 driver identity — the `register` entry and
the §18.3 `BIND_KEYS` bind table. The reusable decode/console/orchestration
logic described below lives in the [`rustos-hid`](../lib/hid.md) library
(`lib/hid`), so the user-space keyboard driver process consumes it without a
`drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4 / §2.2).

The USB-HID logic decodes the two **boot-protocol** report formats
(USB HID 1.11 Appendix B) — the fixed 8-byte keyboard report and the
3-or-more-byte mouse report every USB keyboard/mouse must speak without
a report-descriptor parse — into platform-neutral `InputEvent`s. It is
the input path for the Pi 4's USB ports (`plans/PI.md` P10).

The decoders (`BootKeyboard`, `BootMouse`) are written against the
`ReportSource` seam, defined in `lib/abi`
(`rustos_abi::driver::input`) because its producer is a sibling driver
and drivers depend only on `lib/*` (`AGENTS.md` §17.4): on metal the
source is the device's interrupt-IN endpoint serviced by the xHCI
driver's `UsbDevice` engine ([bus drivers](bus.md)), which enumerates
the device (`SET_PROTOCOL(boot)` included) and polls the interrupt-IN
transfer ring; host tests drive a mock report queue — the
`emmc2`/`rpi_hvs` seam shape (`AGENTS.md` §2.2) — and the usb crate's
end-to-end test polls a `BootKeyboard` over the mock controller. The
PCI BAR / hwtree wiring for the VL805 is the remaining P10 work; QEMU
models no Pi USB timing, so the host suite is the emulation artefact
and metal acceptance is a checklist.

The keyboard report carries *state* (every held key appears in every
report), so the decoder diffs consecutive reports and emits one `Key`
edge per change — releases, then presses, then modifier-bit edges —
with `code` the HID usage ID (page `0x07`; modifiers `0xE0..=0xE7`). A
rollover/POST-error report (an array slot in `0x01..=0x03`) keeps the
held-key state and diffs only the still-valid modifier byte; a
duplicated usage in a hostile report presses once. Mouse reports are
buttons (diffed into `Key` events `0x110`/`0x111`/`0x112` — the same
codes a virtio pointer device delivers) plus `Pointer` X/Y and `Scroll`
wheel deltas.

Everything fails closed (`AGENTS.md` §5.4): wrong-length reports are
rejected whole (`LengthOutOfRange`) without touching the device state,
a source claiming more bytes than its buffer is a `DeviceFault`, and
events that overflow the caller's `poll` buffer are latched for the
next call rather than dropped. A per-`poll` report budget bounds the
work a flooding device can force (`AGENTS.md` §2.1).

#### Console-input producer

For a keyboard wired to a text console, the driver's `console` module
turns the raw HID-usage edges above into the console (tty) bytes a
terminal sends (`plans/PI.md` P11). `KeyboardConsole` tracks the held
modifiers and the caps-/num-lock state, resolves each press to the
`rustos_input::Key` a US layout produces — the HID-usage table is
HID-specific, so it lives in the driver; a `ps2` keyboard resolves
scancode set 1 into the same vocabulary — and runs that key through the
shared `lib/keymap` terminal map ([`rustos-keymap`](../lib/keymap.md)),
the one definition that owns the `Key`→bytes translation
(`AGENTS.md` §2.2). `pump_once` is the driver loop: poll the keyboard,
feed each event, and inject the produced bytes through a `ConsoleSink`
— on metal a `console_input` call against the video console's index,
host-tested with a recording sink. The whole path is allocation-free
and fails closed (an unknown usage or a non-press produces no bytes).
Delivering the reports over the Pi 4's VL805 xHCI controller is the
remaining metal step; QEMU models no Pi USB, so the decode + keymap are
host-proven and the hardware delivery is a checklist.

#### Boot-keyboard driver-process orchestration

The `service` module is the composition a **user-space** USB boot-keyboard
driver runs at start-up (`plans/PI.md` P10 chunk 5d-2-ii). It is
arch-neutral: the board `PCIe` root-complex bring-up and BAR assignment stay
in the separate board bus driver (`lib/pcie_brcm` +
`drivers/bus/usb`); the keyboard driver is autoloaded against the discovered
HID node, granted **only** the resources its matched node requested — its
already-assigned xHCI register BAR and a DMA constraint (`AGENTS.md` §18.3) —
and reaches them through the `DriverHost` its runtime builds over those
grants. So the orchestration names no PCI, no BCM2711, and no board
(`AGENTS.md` §2.20).

`bring_up_boot_keyboard(host, delay, bar_base, bar_len, dma_aperture_top)`
carves the device-shared DMA region and checks it lies wholly below the
discovered inbound-DMA aperture **before** any register is touched (fail
closed, `AGENTS.md` §5.4), maps the granted register BAR, brings the
controller up (`rustos_usb::Xhci::open` + `UsbDevice::start`), and runs the
arch-neutral root→hub→downstream-HID enumeration
(`UsbDevice::enumerate_boot_keyboard`, which descends the Pi 4's onboard hub
when present, `AGENTS.md` §2.2). It returns a `BootKeyboard` the driver's
service loop drives with `pump_once`, injecting each decoded key edge through
`key_inject`. The device-shared DMA size is the one `rustos_usb::XHCI_DMA_BYTES`
the xHCI driver's wiring also carves (`AGENTS.md` §2.2). QEMU models no Pi USB
timing, so the host tests prove the composition and its fail-closed paths up
to the controller hand-off — over an inert mock window `Xhci::open` fails
closed with `DeviceFault`, the on-metal boundary — and the live bring-up plus
the report pump are the metal acceptance item.

`derive_keyboard_resources` turns the device-resource grants the kernel
delivered (its `HwResource` set) into the `bar_base`/`bar_len`/
`dma_aperture_top` the bring-up needs: exactly one register window — an
`Mmio` window (named by its base) or an outbound `BusWindow` (named by its
far-side bus address) — and exactly one `Dma` constraint (its device-visible
exclusive top: the far-side base plus extent for a translated inbound
viewport, or its `addr_limit` for an untranslated one). It fails closed
(`NotFound` for a missing window or constraint, `Unsupported` for an
ambiguous double grant, `OutOfRange` for a zero-length BAR) and never guesses
a board constant (`AGENTS.md` §2.16 / §2.20 / §5.4).

#### The autoloaded driver binary (`rustos-drv-input-usb-kbd`)

The keyboard driver *process* is a **separate crate**,
`drivers/input/usb_kbd` (`rustos-drv-input-usb-kbd`, `src/main.rs`): the
`devmgr`-autoloaded **user-space** keyboard driver, installed as a signed
`/System/Drivers/` bundle (`AGENTS.md` §18, `plans/PI.md` P10 chunk
5d-2-ii) — the "drivers in user space" steady state (`AGENTS.md` §4). It is a
pure-Rust `rustos-rt` program (`AGENTS.md` §1 / §16.4) kept separate from the
`rustos-drv-input-usb-hid` driver so the userland runtime never enters the
kernel's dependency graph, and depends only on `lib/*` crates so the §17.4
layering holds. `main` builds `rustos_drvrt::RtDriverHost::from_grants_query`
over its kernel-issued grants (coherent DMA is carved kernel-side, so no
architecture-specific cache shim is supplied — platform-neutral, `AGENTS.md`
§2.20), derives its BAR + DMA aperture from the same grants with
`rustos_hid::derive_keyboard_resources` (no second `resource_grants` syscall,
`AGENTS.md` §2.16), runs `rustos_hid::bring_up_boot_keyboard`, and then polls
the keyboard forever with `rustos_hid::pump_once`, injecting each decoded key
edge into the kernel input-focus arbiter through the `key_inject` syscall and
yielding between polls (`AGENTS.md` §2.1). The host adds no authority — every
capability and bound is re-checked kernel-side (`AGENTS.md` §5.4) — and a
bring-up failure exits with a reserved fail-closed code, leaving the console
without a keyboard rather than wedged (`AGENTS.md` §2.9). This bundle is
installed into the image `/System/Drivers/` store and autoloaded by `devmgr`
against the discovered `usb,xhci` node the VL805 bus driver emits (the
recursive bus chain, `plans/PI.md` P10 D5d); QEMU models no Pi USB, so the
live autoload + keystroke is the metal acceptance item (`AGENTS.md` §0.9).
