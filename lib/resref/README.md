# rustos-resref

Shared resource-reference parser for RustOS (`lib/resref`).

RustOS has no `/dev`, `/proc`, or `/sys`. Non-filesystem resources — the random
source, a disk, a serial port, a live metric — are named by typed *resource
references* such as `sys:random`, `disk:backup@7K2M`, or
`stats:net/wan/rx.pps?window=1s`. Several components need to turn such a string
into a structured, validated form: the shell first (redirection targets, command
arguments, completion, typed shell values), and the resolver services and ABI
helpers behind it. That lexing is identical wherever it happens, so it lives here
once and every consumer imports it, rather than each embedding a private
reference parser. The shell does not own a reference parser; it links this one.

## A spelling step, not a resolver

This crate turns a string into a typed `ResourceRef`, and owns two more
pieces of pure registry/spelling policy every consumer shares: the
path-versus-reference decision (`classify_target`, with its structural
half `names_resource_reference` — the one rule that says `sys:random` is a
reference while `Home:/x`, `./sys:random`, or `foo:bar` stay paths, applied
by the shell's redirection targets and by the userland runtime's
open-by-name path, `rustos_rt::File::open`, so every tool's file operands
accept a reference without tool-side code), and the closed registry views
(`KnownNamespace::ALL` and
`well_known_selectors`, the display/completion data the kernel resolver's
tests cross-check against what it actually serves). It does **not** resolve a
namespace to a resource, open anything, check an identity fingerprint, or perform
a capability check. Resolution is capability-checked, fail-closed, and owned by
the resolver services (the System Information API for `info:`/`stats:`, the device
manager and hardware tree for device namespaces). Parsing a string here can never
widen authority, and the resolver-level errors (`UnknownNamespace`,
`CapabilityDenied`, `IdentityMismatch`, …) are not produced here — this layer only
reports whether a string is a well-formed reference.

## Grammar

```text
resource-ref = namespace ":" selector [ "@" guard ] [ "::" facet ] [ "?" params ]
selector     = [ selector-part *( "/" selector-part ) ]
params       = param *( "," param )
param        = key op value
op           = "=" | "!=" | "<" | "<=" | ">" | ">=" | "~"
```

- `namespace`, `facet`, and parameter `key` are lowercase ASCII idents
  (`a-z 0-9 - _`) starting with a letter.
- Selector parts are case-sensitive (`a-z A-Z 0-9 - _ .`), because full identity
  selectors carry mixed-case tokens (`disk:id/serial/S6XYZ123456789`).
- The reserved delimiters `:`, `/`, `@`, `::`, `?`, and `,` never appear inside
  the part they delimit, so a rendered reference always re-parses.
- `@guard` may stand alone (`disk:@7K2M`, the short-fingerprint shorthand) and a
  `?params` query may stand alone (`disk:?removable=true`). A reference with an
  empty selector and no guard or query (`disk:`) is rejected.

The closed namespace registry (`sys`, `info`, `stats`, `state`, `disk`, `part`,
`vol`, `tty`, `net`, `input`, `audio`, `gpu`, `bus`, `svc`, `proc`, `cap`) is
defined here once as `KnownNamespace`; a well-formed but unregistered namespace
still parses (membership is a resolver decision, not a syntax error).

## API

- `parse(input) -> Result<ResourceRef, RefError>` — the only fallible step.
- `ResourceRef::namespace() / selector() / guard() / facet() / params()` and a
  `Display` that renders the canonical spelling.
- `Namespace::as_str() / known()`, `KnownNamespace::as_str() / from_name()`.
- `Param::key() / op() / value()`, `Op`.
- `RefError` — why a string was rejected.
- `MAX_REF_LEN` / `MAX_NAMESPACE_LEN` / `MAX_SELECTOR_SEGMENTS` /
  `MAX_SEGMENT_LEN` / `MAX_GUARD_LEN` / `MAX_FACET_LEN` / `MAX_PARAMS` /
  `MAX_PARAM_KEY_LEN` / `MAX_PARAM_VALUE_LEN` — the fixed security bounds on an
  untrusted reference string.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`.
- Fail-closed: a malformed or over-long reference is a typed `RefError`, never a
  silently "fixed up" value. It parses untrusted input, so every dimension is a
  fixed security bound, not a growable capacity.
- Linear time, no recursion: neither a hostile reference nor a hostile segment
  can trigger runaway work. Parsing never panics.

## Stability

Tier: `experimental` (`abi-v1` is not frozen; the surface grows as callers need
it).
