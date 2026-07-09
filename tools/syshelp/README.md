# rustos-syshelp

Build-discovered **system app-store Help payload** for image authoring.

Stability tier: **experimental** (host build tooling).

RustOS ships each command app's internationalised command help as a
structured-Markdown `Help/` tree on the read-only `/System` volume, at
`/System/Apps/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`). The image
builder (`tools/mkimage`) and the QEMU image fixture must plant that tree onto
the volume they author.

This crate's `build.rs` walks the command-app source roots
(`userland/apps/*/Help/` and `userland/apps/*/Resources/`), finds every help
document and bundle resource, and embeds each as a row in `HELP_FILES` /
`RESOURCE_FILES`. The image builder iterates that data to plant the payload —
it never carries a hand-maintained per-bundle list, and no help text or
resource data is hardcoded into an app binary. The source of truth is the
bundle's own on-disk `Help/` / `Resources/` directory; adding a bundle's
payload is dropping files on disk, and the next build rediscovers them
(`AGENTS.md` §2.2, the §16.5 self-contained-bundle rule). A resource (e.g.
`lspci.app`'s compiled `pci.ids.bin` lookup table) is planted at
`Apps/<bundle>/Resources/<file>` and covered by the bundle's signed `AppInfo`
content hash, so a tampered resource fails the load gate closed.

The payload is `&'static [u8]` bytes embedded at build time, so the crate is
`no_std` and depends on no app crate: both the host image builder and the
freestanding QEMU fixture (which also links into the aarch64 guest tail)
consume it unchanged.
