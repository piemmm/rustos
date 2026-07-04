# rustos-syshelp

Build-discovered **system app-store Help payload** for image authoring.

Stability tier: **experimental** (host build tooling).

RustOS ships each command app's internationalised command help as a
structured-Markdown `Help/` tree on the read-only `/System` volume, at
`/System/Apps/<name>.app/Help/<locale>/<doc>.md` (`plans/APPS.md`). The image
builder (`tools/mkimage`) and the QEMU image fixture must plant that tree onto
the volume they author.

This crate's `build.rs` walks the command-app source roots
(`userland/apps/*/Help/`), finds every help document, and embeds each as a row
in `HELP_FILES`. The image builder iterates that data to plant help — it never
carries a hand-maintained per-bundle list, and no help text is hardcoded into
an app binary. The source of truth for a command's help is the bundle's own
on-disk `Help/` tree; adding a bundle's help is dropping files on disk, and the
next build rediscovers them (`AGENTS.md` §2.2, the §16.5 help-authoring rule).

The payload is `&'static [u8]` bytes embedded at build time, so the crate is
`no_std` and depends on no app crate: both the host image builder and the
freestanding QEMU fixture (which also links into the aarch64 guest tail)
consume it unchanged.
