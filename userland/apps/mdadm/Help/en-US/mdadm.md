## NAME

mdadm — inspect and administer RAID arrays

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Inspects and administers the software RAID arrays the array composer
assembles from member devices. The array and device inventory is read
through the System Information API — the same interface, at the same
`CAP_SYSINFO_HW` bar the hardware tree is read under. The create, add,
remove, and stop mutations are posted to the composer's control
endpoint, which checks the caller holds `CAP_STORAGE_ADMIN` before it
acts. A refusal is reported on standard error with a non-zero exit;
nothing is fabricated and no authority is assumed.

Exactly one mode is given per invocation.

TAIRiX has no `/dev`, so the two names Linux mdadm spells as device
files are spelled differently here — a deliberate, documented
divergence:

- A device is named by its hardware-tree node id, written `node:<id>`,
  the same name the reports print. Any other spelling is refused rather
  than guessed at.
- An array is named by its 128-bit identity in hexadecimal. The full
  32-digit identity is accepted, as is any prefix that names exactly one
  array; a prefix matching more than one array is refused rather than
  guessing which was meant.

TAIRiX composes RAID levels 0, 1, 5, 6, 10, and triple parity. It has no
RAID4, so `--level=4` is refused with that reason.

Concise advisory context — a degraded array, or blank devices not shown
in the array view — is written to the standard information stream
(fd 3). It is optional and never changes the primary output.

## OPTIONS

- `-C, --create` — create an array over the named devices and print the
  identity the composer mints for it.
- `-D, --detail` — report each array's identity, level, health, device
  counts, geometry, and any rebuild or verification position. With no
  array operand, report every array.
- `-E, --examine` — list every device the composer holds: array members
  with their slot and state, and the unaffiliated blank devices a new
  array can be created over.
- `-a, --add` — admit a blank device into an array's absent slot and
  rebuild it.
- `-r, --remove` — retire a member device from an array.
- `-S, --stop` — stop a live array and release its members.
- `-l, --level=<level>` — the level to create: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, or
  `tp`/`raid-tp` for triple parity.
- `-n, --raid-devices=<count>` — the number of member slots to create;
  it must equal the number of device operands.
- `-c, --chunk=<blocks>` — the stripe unit in logical blocks; valid only
  for a striped level.
- `-h, -?, --help` — show this command's own help.
- `-V, --version` — print the version and exit.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — create a RAID5 array over three devices.
- `mdadm --detail` — report every array.
- `mdadm --examine` — list every device, members and blanks alike.
- `mdadm --add 3f2a node:14` — add a device to the array whose identity begins `3f2a`.
- `mdadm --stop 3f2a` — stop that array.

## EXIT STATUS

- `0` — the request succeeded (or the help was written).
- `1` — a capability was refused, a name did not resolve, the composer
  refused the request, or the output could not be written.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for this help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
