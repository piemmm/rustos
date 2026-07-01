# `rustos-resref` — resource-reference parser

`rustos_resref` (`lib/resref`) is RustOS's one definition of how a *resource
reference* string is lexed and validated into a structured value. RustOS has no
`/dev`, `/proc`, or `/sys`; non-filesystem resources — the random source, a
disk, a serial port, a live metric — are named by typed references such as
`sys:random`, `disk:backup@7K2M`, or `stats:net/wan/rx.pps?window=1s`. Turning
that spelling into a typed form is identical wherever it happens (the shell's
redirection targets, command arguments, completion, and typed shell values
first, and the resolver services behind them), so it lives here once and every
consumer imports it rather than embedding a second reference parser.

Stability tier: **experimental** (the surface grows as callers need it).

## A spelling step, not a resolver

The crate turns a `&str` into a typed `ResourceRef`. It performs **no**
resolution, I/O, or authority check: it does not map a namespace to a resource,
does not open anything, does not verify an identity fingerprint, and adds no
syscall, capability, or `lib/abi` type. Resolution is capability-checked,
fail-closed, and owned by the resolver services — the System Information API for
`info:`/`stats:`, the device manager and hardware tree for the device
namespaces. Because parsing only inspects a string, it can never widen
authority, and the resolver-level errors (`UnknownNamespace`, `CapabilityDenied`,
`IdentityMismatch`, `StaleGeneration`, …) are decided there, not here.

## Grammar

```text
resource-ref = namespace ":" selector [ "@" guard ] [ "::" facet ] [ "?" params ]
selector     = [ selector-part *( "/" selector-part ) ]
params       = param *( "," param )
param        = key op value
op           = "=" | "!=" | "<" | "<=" | ">" | ">=" | "~"
```

| Part | Example | Notes |
| --- | --- | --- |
| Simple | `sys:random` | one-segment selector |
| Multi-segment | `info:cpu/vendor` | `/`-separated selector parts |
| Guarded | `disk:backup@7K2M` | identity-guard fingerprint |
| Faceted | `disk:backup@7K2M::raw` | typed operation/view |
| Query | `stats:net/wan/rx.pps?window=1s` | comparison parameters |
| Query-only | `disk:?removable=true,size>=16GiB` | empty selector + query |
| Fingerprint shorthand | `disk:@7K2M` | empty selector + guard |
| Full identity | `disk:id/serial/S6XYZ123456789` | case-sensitive selector |

`namespace`, `facet`, and each parameter `key` are lowercase ASCII idents
(`a-z 0-9 - _`) that start with a letter. Selector parts are **case-sensitive**
(`a-z A-Z 0-9 - _ .`), because full identity selectors carry mixed-case,
hyphenated tokens. The reserved delimiters `:`, `/`, `@`, `::`, `?`, and `,`
never appear inside the part they delimit, so a rendered reference always
re-parses to the same value (`Display` renders the canonical spelling).

The closed namespace registry — `sys`, `info`, `stats`, `state`, `disk`,
`part`, `vol`, `tty`, `net`, `input`, `audio`, `gpu`, `bus`, `svc`, `proc`,
`cap` — is defined here once as `KnownNamespace`. A well-formed but unregistered
namespace still parses; membership is a resolver decision, not a syntax error,
so classifying it (`Namespace::known`) is separate from parsing it.

## Boundary with filesystem paths

A string with **no** `:` delimiter is refused with `RefError::NotAReference`:
that is a filesystem *path* (`/Users/ian`, `Documents/spec.md`), owned by the
separate `lib/path` grammar. The two grammars stay one definition each — a
caller routes a `NotAReference` string to the path parser, and `lib/path` routes
its `NotAPath` (a `Name:selector` shape) here.

## Fail closed, bounded, and round-tripping

A reference string is untrusted input, so every dimension is a fixed
**security** bound, not a growable capacity: `MAX_REF_LEN`,
`MAX_NAMESPACE_LEN`, `MAX_SELECTOR_SEGMENTS`, `MAX_SEGMENT_LEN`, `MAX_GUARD_LEN`,
`MAX_FACET_LEN`, `MAX_PARAMS`, `MAX_PARAM_KEY_LEN`, and `MAX_PARAM_VALUE_LEN`.
Parsing fails closed with a typed `RefError` — never a silently "fixed up"
value — when a bound is exceeded, a token is empty or malformed, a delimiter is
misplaced (a guard after a facet, an empty selector with no guard or query), or
a disallowed character appears.

The crate is `no_std` + `alloc` and `#![forbid(unsafe_code)]`. Parsing is the
only fallible step and never panics; it runs in time linear in the input with no
recursion, so neither a hostile reference nor a hostile segment can drive
runaway work. The `fuzz_resref` harness checks that any byte string either
parses or fails closed and that every parsed reference's canonical spelling
re-parses to an equal value.
