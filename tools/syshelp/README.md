# tairix-syshelp

Build-discovered **system payload** for image authoring: the command apps'
Help documents and bundle resources, plus the desktop's graphics assets.

Stability tier: **experimental** (host build tooling).

TAIRiX ships each command app's internationalised command help as a
structured-Markdown `Help/` tree on the read-only `/System` volume, at
`<store>/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`) — where `<store>`
is the bundle's own declared store — each app's bundle resources at
`<store>/<name>.app/Resources/<file>`, and the
desktop's graphics assets — the raster icon masters — under `/System/Graphics`
(`AGENTS.md` §16.2, §10). The image builder (`tools/mkimage`) and the QEMU
image fixture must plant all of these onto the volume they author.

This crate's `build.rs` walks the command-app source roots
(`userland/{apps,gui,shell}/*/Help/` and `.../Resources/`) and the desktop
icon directory (`lib/icon/assets/`), finds every file, and embeds each as a
row in `HELP_FILES` / `RESOURCE_FILES` / `GRAPHICS_FILES`. Both planters drive
their own `plant_nested_file` from the one shared walk `plant_system_payload`,
so they can never lay down a different set of files or spell a path
differently. The payload is never a hand-maintained list, and no help text,
resource, or icon is hardcoded into a binary. The source of truth is each
family's own on-disk directory; adding a help document, a resource, or an icon
(`<asset-id>.png`) is dropping files on disk, and the next build rediscovers
them (`AGENTS.md` §2.2, the §16.5 self-contained-bundle rule).

- A resource (e.g. `lspci.app`'s compiled `pci.ids.bin` lookup table) is
  planted at `Apps/<bundle>/Resources/<file>` and covered by the bundle's
  signed `AppInfo` content hash, so a tampered resource fails the load gate
  closed.
- A graphics asset is planted at `Graphics/<dir>/<file>` (today `dir` is
  `Icons`). Each is validated against the desktop's own icon contract
  (`tairix_icon`) as it is discovered — a legal `<asset-id>.png` name, within
  the `MAX_ARTWORK_BYTES` bound, with a unique asset id — so a name the
  desktop could never resolve or an over-large file fails the build closed
  rather than shipping artwork that would silently render as a fallback glyph.

The payload is `&'static [u8]` bytes embedded at build time, so the crate is
`no_std` and depends on no app crate: both the host image builder and the
freestanding QEMU fixture (which also links into the aarch64 guest tail)
consume it unchanged.
