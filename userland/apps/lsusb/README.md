# `tairix-lsusb` — list discovered USB devices

A `plans/DEVICES.md` DEVICE1 command app (`AGENTS.md` §3
`userland/apps/`), registered as the system app store bundle `lsusb.app`
so the shell resolves the bare word `lsusb` to it. `lsusb` lists, one
line per discovered USB interface, the interface's bus and device
numbers, its `vendor:product` id, and its vendor and product names. The
option surface follows `usbutils` over what the TAIRiX model actually
carries (`AGENTS.md` §16.7): `-v` interface class identity, `-t` bus
topology, `-d [<vendor>]:[<product>]` and `-s [[<bus>]:][<devnum>]`
filters. `-?`/`--help` render the tool's own short help from its bundled
`Help/` tree through the shared `lib/help` engine (`plans/APPS.md` §4).

The inventory is read exclusively through the System Information API
(`AGENTS.md` §16.6): the `CAP_SYSINFO_HW`-gated `sysinfo-v1`
`HARDWARE_TREE` query served by `sysinfod`, paged in whole through the
shared `tairix_procinfo::hwtree::fetch_tree` walk — fail-closed whole
`tairix_abi::hwtree::HwNode` records reassembled from a
generation-checked snapshot, never a `/proc`-style scrape and never a
kernel bypass. A refused query defeats the tool's whole purpose, so
the reason lands on standard error and nothing is fabricated
(`AGENTS.md` §2.24).

Names come from the vetted `usb.ids` snapshot (`lib/devids`) compiled
into the compact table this bundle ships as `Resources/usb.ids.bin` —
data on the volume, read at runtime through the secured VFS, never
`include_bytes!` into the binary (`AGENTS.md` §16.5). An identity the
database does not name shows only its numeric `ID vvvv:pppp` form
(exactly as `usbutils` omits an unknown string), never fabricated, with
the count advised on fd 3 (`usb.names_unresolved`, `AGENTS.md` §20.1);
a missing or invalid table degrades the whole listing to bare ids with
the reason on standard error — the inventory itself is never withheld
over a naming aid.

Documented divergences from Linux `usbutils` (`AGENTS.md` §16.7,
divergence-by-concept): TAIRiX has no Linux bus/devnum registry, so a
device's bus number is its controller's stable hardware-tree node id
and its device number is its own node id, and `-s` selects those node
ids; the inventory records one node per *interface*, so a
multi-interface device lists once per interface and no root-hub
pseudo-devices are shown — the tool reports only what the system
actually records (`plans/DEVICES.md` §1.4).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-devids`, `tairix-help`, and `tairix-procinfo` crates. The pure
engine (`src/client.rs`) is host-tested over injected seams — a canned
hardware tree and a fixture database compiled through the real
`lib/devids` import pipeline; the freestanding `Run` binary
(`src/run.rs`) binds the production seams over `tairix-rt`.
