# tairix-path

Shared filesystem path-spelling parser for TAIRiX (`lib/path`).

TAIRiX storage is a forest of named roots, not one global Unix tree, so a path
string names its root explicitly. Several components need to turn such a string
into a structured, validated form — the shell first (`cd`, prompt display, word
and tilde expansion, completion), and later the file browser and file-management
tools. That lexing and normalisation is identical wherever it happens, so it
lives here once and every consumer imports it, rather than each embedding a
private path parser. The shell does not own a path parser; it links this one.

## A spelling step, not a resolver

This crate turns a string into a typed `Path` (a `Root` plus normalised
components). It does **not** resolve an alias to a volume, open anything, or
perform a capability check. A resolved name is still subject to inode
permissions, ACLs, capability gates, mount flags, and MAC policy at open time.
Resolving a root handle and opening a descriptor belong to the storage and
filesystem subsystems, so parsing a string here can never widen authority.

## Accepted spellings

- **Synthetic view** — `/Users/ian/Documents` (`Root::View`).
- **Alias shorthand** — `Home:/Documents/spec.md` (`Root::Alias`). The first `/`
  after the `:` marks the root boundary; the alias name is not a component.
- **Expanded alias** — `alias::Home/Documents` (the normalised internal spelling
  of the same alias path; parses to the identical `Root::Alias`).
- **Relative** — `Documents/spec.md`, `.`, `../notes` (`Root::Relative`),
  resolved by the caller against a current directory.

A leading `Name:` (single colon) not followed by `/` is refused with
`PathError::NotAPath`: that is a resource reference (`namespace:selector`, owned
by the separate resource-reference grammar) or an alias path missing its `/`.
The durable/administrative resolver spellings (`id::`, `fs::`, `<driver>::`,
`dev::`, `net::`) are refused with `PathError::UnsupportedResolver` — they have
no consumer yet and are added by the stage that introduces their callers, rather
than being invented speculatively here.

## API

- `parse(input) -> Result<Path, PathError>` — the only fallible step.
- `Path::root() / components() / alias() / is_absolute()` and a `Display` that
  renders the canonical human spelling.
- `PathError` — why a string was rejected.
- `MAX_PATH_LEN` / `MAX_COMPONENTS` / `MAX_COMPONENT_LEN` / `MAX_ALIAS_LEN` — the
  fixed security bounds on an untrusted path string.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`.
- Fail-closed: a malformed or over-long path is a typed `PathError`, never a
  silently "fixed up" path. It parses untrusted input, so length, component
  count, component size, and alias size are fixed security bounds, not growable
  capacities.
- `..` cannot climb above a view or alias root (`EscapesRoot`); a leading `..`
  in a relative path is preserved for the caller to resolve.
- Linear time, no recursion: neither a hostile path nor a hostile component can
  trigger runaway work. Parsing never panics.

## Stability

Tier: `experimental`.
