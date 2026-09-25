# tairix-syshelp

Build-discovered **system payload** for image authoring, and the one
definition both disk authors build their disk through.

Stability tier: **experimental** (host build tooling).

## Payload

TAIRiX ships on the read-only `/System` volume each command app's
internationalised `Help/` tree at `<store>/<name>.app/Help/<locale>/<doc>.md`
(`plans/APPS.md`), each app's bundle resources at
`<store>/<name>.app/Resources/<file>` (`<store>` being the store the bundle's
own manifest kind installs it to), and the desktop's graphics assets (icon
masters, wallpaper masters, cursor sets) under `/System/Graphics`.

`build.rs` walks the command-app source roots (`userland/{apps,gui,shell}`)
and each graphics family's own directory (`lib/icon/assets/`,
`lib/wallpaper/assets/`, `lib/cursor/assets/`), and embeds every file it finds
as a row of `HELP_FILES`, `RESOURCE_FILES` or `GRAPHICS_FILES`. Adding a help
document, a resource or an asset is dropping a file on disk; nothing keeps a
hand-maintained list, and nothing hardcodes the payload into a binary.

- A resource (e.g. `lspci.app`'s compiled `pci.ids.bin` lookup table) is
  covered by the bundle's signed `AppInfo` content hash, so a tampered
  resource fails the load gate closed.
- Each graphics asset is validated against its own family's contract as it is
  discovered, so a name its consumer could never resolve or an over-large file
  fails the build closed instead of shipping artwork that never draws.

## The disk

The image builder (`tools/mkimage`) and the QEMU whole-disk fixture
(`tests/integration/encrypted_root_image`) both build through this crate, so
neither keeps its own copy of the file set, the sizing or the layout.

- `build_system_volume` counts and plants the payload and the caller's
  bundles in one walk, through the caller's `SystemVolume`, into a volume of
  whole `SYSTEM_VOLUME_GRAIN_BYTES` grains: the planted bytes rounded up, plus
  one grain whenever the filesystem's own metadata does not fit.
- `assemble_disk` lays the boot, `/System` and root partitions back to back
  from `BOOT_PART_LBA` behind an MBR, refusing a partition that is not whole
  `SECTOR_BYTES` sectors or that the table cannot describe.

The payload is `&'static [u8]` bytes embedded at build time, so the crate is
`no_std` (with `alloc`) and depends on no app crate: both the host image
builder and the freestanding QEMU fixture consume it unchanged.
