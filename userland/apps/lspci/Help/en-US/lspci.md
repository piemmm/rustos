## NAME

lspci — list discovered PCI/PCIe devices

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Lists, one line per discovered PCI/PCIe function, the function's
hardware-tree node id, its class, and its vendor and device names. The
inventory is the hardware tree — the system's single device inventory —
read through the System Information API, which requires the
`CAP_SYSINFO_HW` capability; a refusal is reported on standard error
and nothing is listed in its place.

Names come from the vetted snapshot of the public PCI ID database this
command ships inside its own bundle. An identity the database does not
name is shown in its numeric form (`Vendor 8086`, `Device 2922`,
`Class 0106`), never invented, and the number of such devices is noted
on the standard information stream (fd 3). If the bundled table is
missing or fails validation, the listing degrades to numeric ids with
the reason on standard error — the inventory itself is still listed.

RustOS records no PCI `bus:device.function` address: a function's
stable address is its hardware-tree node id, shown as `#<node>`, and
`-s` selects that node id (a deliberate, documented divergence from
Linux's `lspci`). The `-k` kernel-driver view is not offered yet: the
system does not publish driver-binding records, and `lspci` reports
only what the system actually records.

## OPTIONS

- `-n` — numeric ids only: the class code and `vendor:device` in hex.
- `-nn` — names followed by the numeric ids in brackets.
- `-v` — after each function, list the resources its tree node
  declares (MMIO windows, IRQ lines, I/O ports, DMA constraints) —
  the capability-grant requests the tree records, not live state.
- `-t` — render the functions as a tree under their bus parents.
- `-d [<vendor>]:[<device>]` — list only functions matching the given
  vendor/device ids (hex); an omitted half matches any.
- `-s <node>` — list only the function with the given hardware-tree
  node id (decimal).
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `lspci` — every discovered PCI function, with names.
- `lspci -nn` — the same, with the numeric ids alongside.
- `lspci -v -s 7` — node 7's line plus its declared resources.
- `lspci -d 1af4:` — every function from vendor `1af4` (virtio).
- `lspci -t` — the functions under their bus topology.

## EXIT STATUS

- `0` — the listing (or the short help) was written.
- `1` — the hardware-tree query was refused or failed, or the output
  could not be written.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
