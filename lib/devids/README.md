# tairix-devids

PCI/USB ID-database engine for TAIRiX (`lib/devids`). Stability tier:
**experimental**.

The `lspci` and `lsusb` command apps (plans/DEVICES.md DEVICE1) name the
devices the hardware tree discovers. The names come from the public PCI
(`pci.ids`, pci-ids.ucw.cz) and USB (`usb.ids`, linux-usb.org) ID databases,
imported as vetted, provenance-pinned snapshots and compiled into compact
binary lookup tables. This crate is the one definition of that pipeline —
grammar, vetting filter, table format, encoder, and decoder — shared by the
`cargo xtask devids` generator, the CI drift gate, and the runtime consumers.

## Pipeline

1. `cargo xtask devids --fetch` (developer-run only, never CI or the build)
   downloads both databases, runs the vetting filter, and rewrites the
   committed snapshots under `lib/devids/assets/` with a provenance header
   (upstream URL/version/date, fetch date, SHA-256 of the raw download,
   licence statement). The refresh diff is human-reviewed like any other
   change.
2. `cargo xtask devids --write` regenerates the compact tables from the
   committed snapshots. Each table is written into its consuming command
   bundle's `Resources/` directory
   (`userland/apps/lspci/Resources/pci.ids.bin`,
   `userland/apps/lsusb/Resources/usb.ids.bin`), so it ships inside the
   self-contained bundle with no second copy in the tree.
3. `cargo xtask devids` (no flag; part of `cargo xtask ci`) re-runs the
   converter and fails closed on any drift between snapshot and tables.

The generated `.bin` tables are data files a command bundle ships as a
resource (`lspci.app/Resources/pci.ids.bin`); they are never `include_bytes!`d
into a binary.

## Vetting

The raw download is untrusted input whose strings end up on users'
terminals, so `textdb::parse` is strict and fail-closed: the whole file must
match the shared line grammar (never skip-and-continue), names must be valid
UTF-8 with no control characters (no terminal escape injection), ids are
exact-width lowercase hex in every emitted scope, duplicates within a scope
reject the import, and source size, name length, and entry counts are
bounded (fixed security bounds with generous headroom — today's databases
are ~46 000 entries against a 262 144 bound).

PCI subsystem entries and the auxiliary `usb.ids` sections (`AT`, `HID`,
`R`, `BIAS`, `PHY`, `HUT`, `L`, `HCC`, `VT`) are validated but not encoded:
no consumer renders them today (the hardware tree records no subsystem
ids), and an unused table would be speculative surface.

## Lookup

`DevIds::parse` validates a compiled table fail-closed (magic, kind, exact
length, sorted keys, every name slice in-bounds on character boundaries) and
serves allocation-free O(log n) binary-search lookups: `vendor`, `device`,
`class`, `subclass`, `prog_if`. An id the database does not name is `None`;
the caller renders the numeric form rather than fabricating a name.

## Features

- `textdb` (default): the snapshot parser/vetting filter/encoder (needs
  `alloc`). A lookup-only consumer builds with `default-features = false`.

## Testing

Unit tests cover the accept/reject matrix of the vetting filter, encoder
determinism and interning, decoder corruption handling, and the committed
snapshots. `tests/fuzz_devids.rs` is the in-tree fuzz harness
(`cargo xtask fuzz`), driving both the text parser and the table decoder
with seeded hostile inputs.
