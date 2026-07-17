# tairix-fsmeta

The one definition of TAIRiX's extended-file-metadata model: the namespaced
attribute-key grammar, the bounded per-inode attribute store with its
self-identifying on-disk encoding, and the closed foreign-metadata preset
registry. ARXFS, the foreign-filesystem drivers, and the copy/move/archive
tooling all share this crate, so a key written by one is read identically by
another and no conversion logic is duplicated (`AGENTS.md` §2.2).

## What it provides

- **`key`** — the closed, curated `Namespace` set (`user`, `acorn`, `amiga`,
  `atari`, `mac`, `tairix`, `system`, `trusted`), each with a fixed
  `NamespaceAccess` class, and `AttrKey::parse`, a fail-closed validator for a
  key's bytes (UTF-8, no `/` or NUL, `namespace.rest` split, `KEY_MAX` bound).
- **`attr`** — `AttrEntry` / `AttrFlags` / `AttrSet`: an ordered set of unique
  attributes bounded by the fixed security limits (`KEY_MAX`, `VALUE_MAX`,
  `ATTRS_PER_INODE`, `TOTAL_ATTR_BYTES`), with a length-prefixed,
  self-identifying `encode` / `decode` a filesystem driver writes into one
  copy-on-write metadata block.
- **`preset`** — exact, checked conversions between each foreign filesystem's
  native per-file fields and normalised attribute values: Acorn/RISC OS
  (filetype, load/exec, 40-bit centisecond datestamp), Amiga (`hsparwed`
  protection, comment), Atari GEMDOS (attribute bits, FAT date/time), and
  classic Mac (`OSType` type/creator, Finder flags, resource-fork stream key).
  Every `Time64` conversion is checked: an instant the foreign format cannot
  represent fails closed with `TimestampOutOfRange`, never silently truncated.

## What it is *not*

- It never interprets what a value *means* — values are opaque byte strings.
- It stores nothing and touches no device; a filesystem driver owns storage.
- The `VALUE_MAX` bound (3 KiB) is a validation bound, not a growable capacity
  (`AGENTS.md` §24.4). A fork larger than `VALUE_MAX` (e.g. a Mac resource
  fork) is a *named stream* stored through the file-data pipeline, not an
  attribute value; its stream key lives here (`preset::mac::RESOURCE_FORK_KEY`).

## Stability

**experimental.** The on-disk attribute encoding and the preset value
encodings are pre-release and evolve in place until ARXFS's first shipped
release (`AGENTS.md` §2.13).
