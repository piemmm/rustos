# Extended-metadata preset registry

The `lib/fsmeta` preset registry is the single source of truth for how each
foreign filesystem's native per-file metadata maps to and from the normalised
attribute values ARXFS stores (`docs/src/filesystem/arxfs-spec.md` §21). It
lives once and is shared by ARXFS, the foreign-filesystem drivers, and the
copy/move/archive tooling (`AGENTS.md` §2.2). Every conversion is exact and
checked: a value or `Time64` instant the foreign format cannot represent fails
closed with `MetadataNotRepresentable` / `TimestampOutOfRange`, never silently
truncated, wrapped, or guessed (`AGENTS.md` §21).

ARXFS stores every value as opaque bytes and never interprets it; the registry
and the converting tool own interpretation.

## Namespaces

`user`, `rustos`, and the foreign namespaces (`acorn`, `amiga`, `atari`, `mac`)
are ordinary file metadata (file read/write permission). `system` and `trusted`
are privileged (a VFS capability gate). See arxfs-spec §21.1.

## Acorn / RISC OS (ADFS, FileCore)

| key | value encoding |
|---|---|
| `acorn.filetype` | 12-bit filetype as three lowercase hex digits, e.g. `fff` (Text). Absent when the object is typed by load/exec instead. |
| `acorn.loadaddr` | 32-bit load address, eight lowercase hex digits. |
| `acorn.execaddr` | 32-bit exec address, eight lowercase hex digits. |
| `acorn.attr` | `FileCore` access bits: owner letters in `RWLDEP` order, `/`, public letters in `rwe` order (e.g. a locked, publicly readable directory is `RLD/r`). |
| `acorn.datestamp` | RISC OS 40-bit centisecond timestamp (since 1900) as ten lowercase hex digits, stored exactly so it round-trips; convertible to/from `Time64`. |

A filetyped object encodes its type and a timestamp *inside* the load/exec
words (`load >> 20 == 0xFFF`). The registry stores the decoded `acorn.filetype`
**and** preserves the raw load/exec, so a copy back to ADFS reproduces the
native fields byte-for-byte.

## Amiga (AmigaDOS / FFS)

| key | value encoding |
|---|---|
| `amiga.protection` | 8-bit protection mask as the canonical `hsparwed` string (`-` for a clear bit). |
| `amiga.comment` | file comment, opaque bytes, up to 79 bytes (the AmigaDOS limit). |

## Atari (GEMDOS / TOS)

| key | value encoding |
|---|---|
| `atari.attributes` | GEMDOS attribute byte (read-only, hidden, system, volume-label, directory, archive); unknown bits rejected. |
| `atari.gemdos_date` | FAT-style packed date/time (two-second resolution, 1980 epoch), convertible to/from `Time64`. |

Atari TOS uses a FAT-derived on-disk format; the registry keeps Atari
attributes distinct from a generic FAT mapping so intent is not lost.

## Classic Mac OS (HFS / HFS+)

| key | value encoding |
|---|---|
| `mac.type` | four-character type code (`OSType`), e.g. `TEXT`. |
| `mac.creator` | four-character creator code, e.g. `ttxt`. |
| `mac.finderflags` | 16-bit Finder flags, big-endian. |
| `mac.resourcefork` | the resource fork, stored as a *named stream* (arxfs-spec §21.2), not an inline attribute value. |

## RustOS native

| key | value encoding |
|---|---|
| `rustos.origin` | provenance: source filesystem family + volume id, set on import so a later export knows where the metadata came from. |
| `rustos.mime` | optional MIME type hint (advisory; never a security decision). |

## Status

The registry is **experimental** and evolves in place until ARXFS's first
shipped release (`AGENTS.md` §2.13). The `cp`/`mv`/desktop/archive tooling and
the per-family foreign-filesystem driver wiring that consume it are staged
future work (arxfs-spec §18).
