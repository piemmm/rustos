# `rustos-drv-input-usb-hid` — USB-HID boot-protocol input driver

`plans/PI.md` P10 deliverable. This crate is the **driver**: the §8
loadable-module identity for a USB HID boot keyboard or mouse — the single
`register` entry point and the §18.3 `BIND_KEYS` bind table `devmgr` matches a
discovered HID node against.

All the reusable protocol logic — the boot-report decoders (`BootKeyboard`,
`BootMouse`), the console-input producer (`KeyboardConsole`, `pump_once`,
`ConsoleSink`), and the arch-neutral xHCI boot-keyboard orchestration
(`bring_up_boot_keyboard`, `derive_keyboard_resources`) — lives in the
[`rustos-hid`](../../../lib/hid/README.md) library, so it is shared by both the
in-kernel keyboard scaffold (transitional) and the user-space keyboard driver
process (`drivers/input/usb_kbd`) without a `drivers/*`→`drivers/*` dependency
(`AGENTS.md` §17.4 / §2.2), exactly as the bus-agnostic xHCI protocol lives in
`lib/usb` rather than the xHCI driver.

## Supported hardware

| Device class      | Match key (`HwMatchKey::usb`, class only) | Bind priority |
|-------------------|-------------------------------------------|---------------|
| USB boot keyboard | class `0x03_01_01`, vendor/product `0`    | 5             |
| USB boot mouse    | class `0x03_01_02`, vendor/product `0`    | 5             |

`BIND_KEYS` binds any HID boot-protocol keyboard or mouse interface by class
alone (vendor/product wildcard), so any such device autoloads this driver
without its device id being hard-coded (`AGENTS.md` §2.2 / §18.3). It is the
single source of truth the driver's signed-manifest bind table is authored from
and `devmgr` resolves a discovered HID node against.

## Capabilities

Loading requires `CAP_DRV_LOAD`. The driver runs in user space and does not
request `CAP_DRV_KERNEL` (`AGENTS.md` §4 / §8).

## Tests

`cargo test -p rustos-drv-input-usb-hid` covers the `register` capability gate
and the `BIND_KEYS` class-wildcard matching (a non-boot HID interface does not
match). The decode/console/orchestration logic is tested in `lib/hid`.
