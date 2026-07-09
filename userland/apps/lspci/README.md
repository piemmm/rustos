# `rustos-lspci` — list discovered PCI/PCIe devices

A `plans/DEVICES.md` DEVICE1 command app (`AGENTS.md` §3
`userland/apps/`), registered as the system app store bundle `lspci.app`
so the shell resolves the bare word `lspci` to it. `lspci` lists, one
line per discovered PCI/PCIe function, the function's hardware-tree node
id, its class, and its vendor and device names. The option surface
follows `pciutils` over what the RustOS model actually carries
(`AGENTS.md` §16.7): `-n`/`-nn` numeric modes, `-v` declared resources,
`-t` bus topology, `-d [<vendor>]:[<device>]` and `-s <node>` filters.
`-?`/`--help` render the tool's own short help from its bundled `Help/`
tree through the shared `lib/help` engine (`plans/APPS.md` §4).

The inventory is read exclusively through the System Information API
(`AGENTS.md` §16.6): the `CAP_SYSINFO_HW`-gated `sysinfo-v1`
`HARDWARE_TREE` query served by `sysinfod`, decoded fail-closed as whole
`rustos_abi::hwtree::HwNode` records — never a `/proc`-style scrape and
never a kernel bypass. A refused query defeats the tool's whole purpose,
so the reason lands on standard error and nothing is fabricated
(`AGENTS.md` §2.24).

Names come from the vetted `pci.ids` snapshot (`lib/devids`) compiled
into the compact table this bundle ships as `Resources/pci.ids.bin` —
data on the volume, read at runtime through the secured VFS, never
`include_bytes!` into the binary (`AGENTS.md` §16.5). An identity the
database does not name is rendered numerically (`Vendor 8086`,
`Device 2922`), never fabricated, with the count advised on fd 3
(`pci.names_unresolved`, `AGENTS.md` §20.1); a missing or invalid table
degrades the whole listing to numeric ids with the reason on standard
error — the inventory itself is never withheld over a naming aid.

Documented divergences from Linux `pciutils` (`AGENTS.md` §16.7,
divergence-by-concept): RustOS records no PCI `bus:device.function`
triple, so a function's address is its stable hardware-tree node id
(`#<node>`) and `-s` selects that id; subsystem ids are not recorded by
the hardware tree today, so no subsystem lines are shown; `-k` (bound
kernel driver) is not offered until the system publishes driver-binding
records — the tool reports only what the system actually records
(`plans/DEVICES.md` §3).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` vocabulary and the shared
`rustos-devids`, `rustos-help`, and `rustos-procinfo` crates. The pure
engine (`src/client.rs`) is host-tested over injected seams — a canned
hardware tree and a fixture database compiled through the real
`lib/devids` import pipeline; the freestanding `Run` binary
(`src/run.rs`) binds the production seams over `rustos-rt`.
