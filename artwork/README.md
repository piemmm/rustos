# artwork/

Design reference art. Nothing here is shipped in an image or read at
runtime: the shipped desktop artwork lives in `lib/icon/assets/` (raster
icon masters) and `/System/Graphics` on an installed system.

## icons/

Icon masters that have no selectable icon kind, kept as reference until
something can honestly select them.

- `disk-floppy.png` — a floppy-disk drive master. No block driver reports a
  floppy medium, so nothing can select it; it returns to `lib/icon/assets/`
  when removable-media classification can distinguish a floppy from the
  other removable media.
