## NAME

lsusb — list discovered USB devices

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Lists, one line per discovered USB device, the device's bus and
device numbers, its `vendor:product` id, and its vendor and product
names. The inventory is the hardware tree — the system's single device
inventory — read through the System Information API, which requires the
`CAP_SYSINFO_HW` capability; a refusal is reported on standard error
and nothing is listed in its place.

Names come from the vetted snapshot of the public USB ID database this
command ships inside its own bundle. An identity the database does not
name shows only its numeric `ID vvvv:pppp` form, never invented, and
the number of such devices is noted on the standard information stream
(fd 3). If the bundled table is missing or fails validation, the
listing degrades to bare ids with the reason on standard error — the
inventory itself is still listed.

RustOS has no Linux bus/device-number registry: bus and device numbers
are small 1-based orderings of the current inventory (buses in
discovery order, devices in listing order on each bus), stable while
the topology is unchanged, and `-s` selects those rendered numbers (a
deliberate, documented divergence from Linux's `lsusb`). The inventory
records one entry per *interface*; the interfaces of one physical
device are grouped by the device address the host controller reported,
so a multi-interface device lists once.

## OPTIONS

- `-v` — after each device, list every one of its interfaces' class,
  subclass, and protocol (`bInterfaceClass`, `bInterfaceSubClass`,
  `bInterfaceProtocol`) with the names the USB class tables carry.
- `-t` — render the buses, their devices, and each device's interface
  classes as a tree.
- `-d [<vendor>]:[<product>]` — list only devices matching the given
  vendor/product ids (hex); an omitted half matches any.
- `-s [[<bus>]:][<devnum>]` — list only devices matching the given bus
  and/or device numbers (decimal), as rendered in the listing; a value
  without a colon is a device number alone.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `lsusb` — every discovered USB device, with names.
- `lsusb -v` — the same, with each interface's class identity.
- `lsusb -s 2:` — every device on bus 2.
- `lsusb -d 046d:` — every device from vendor `046d` (Logitech).
- `lsusb -t` — the devices under their bus topology.

## EXIT STATUS

- `0` — the listing (or the short help) was written.
- `1` — the hardware-tree query was refused or failed, or the output
  could not be written.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
