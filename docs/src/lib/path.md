# `rustos-path` — filesystem path-spelling parser

`rustos_path` (`lib/path`) is RustOS's one definition of how a path *string* is
lexed and normalised into a structured value. RustOS storage is a **forest of
named roots**, not one global Unix tree, so a path names its root explicitly;
turning that spelling into a typed form is identical wherever it happens (the
shell's `cd`, prompt display, word and tilde expansion, and completion first,
and later the file browser and file-management tools), so it lives here once and
every consumer imports it rather than embedding a second path parser.

Stability tier: **experimental** (the surface grows as callers need it).

## A spelling step, not a resolver

The crate turns a `&str` into a typed `Path` (a `Root` plus normalised
components). It performs **no** resolution, I/O, or authority check: it does not
map an alias to a volume, does not open anything, and adds no syscall,
capability, or `lib/abi` type. A resolved name is still subject to inode
permissions, ACLs, capability gates, mount flags, and MAC policy at open time;
resolving a root handle and opening a descriptor belong to the storage and
filesystem subsystems. Because parsing only rewrites a string, it can never
widen authority.

## Accepted spellings

| Spelling | Example | Root |
| --- | --- | --- |
| Synthetic view | `/Users/ian/Documents` | `Root::View` |
| Alias shorthand | `Home:/Documents/spec.md` | `Root::Alias("Home")` |
| Expanded alias | `alias::Home/Documents` | `Root::Alias("Home")` |
| Relative | `Documents/spec.md`, `.`, `../notes` | `Root::Relative` |

The alias shorthand and the expanded `alias::` form parse to the **identical**
value; the shorthand is the canonical human spelling the `Display` impl renders.
The first `/` after the `:` marks the root boundary — the alias name is never a
path component, and `Alias:relative` (no `/`) is not a filesystem path.

## Boundary with resource references

A leading `Name:` (single colon) **not** followed by `/` is refused with
`PathError::NotAPath`. That shape is a *resource reference*
(`namespace:selector`, e.g. `sys:random`), which belongs to the separate
resource-reference grammar, or an alias path missing its `/`. A caller that also
handles resource references routes a `NotAPath` string to that resolver; this
crate deliberately does not parse resource references, so the two grammars stay
one definition each.

For *lexical* text tools — the `basename`/`dirname` command apps, which
perform POSIX string surgery on a path spelling without resolving or
normalising it — the crate exports `alias_root_len`: the byte length of a
leading `Name:/` alias-root prefix, decided by the grammar's own
root-delimiter rule and alias-name validation, so such a tool treats an alias
root the way POSIX treats `/` (a root that is never stripped into) without a
second path parser.

For the ancestor-walking tools — `mkdir -p` (create missing parents,
outermost first) and `rmdir -p` (remove parents, innermost first) — a parsed
`Path` exports `prefix(len)`: the canonical spelling of the path truncated to
its first `len` components (`prefix(0)` is the bare root — `/`, `Name:/`, or
`.`). Both walks import this one rule, so how a truncated path is spelled is
never re-derived per tool.

The durable and administrative resolver spellings — `id::<volume-id>/…`,
`fs::<driver>/<root>/…`, the `<driver>::<root>/…` shorthand, `dev::…`, and
`net::…` — are refused with `PathError::UnsupportedResolver`. They serve
durable-reference and recovery tooling that does not exist yet; parsing them
now, with no consumer, would be a speculative interface, so they are added by
the stage that introduces their callers.

## Fail closed, bounded, and round-tripping

A path string is untrusted input, so every dimension is a fixed **security**
bound, not a growable capacity: `MAX_PATH_LEN`, `MAX_COMPONENTS`,
`MAX_COMPONENT_LEN`, and `MAX_ALIAS_LEN`. Parsing fails closed with a typed
`PathError` — never a silently "fixed up" path — when a bound is exceeded, an
interior component is empty (`a//b`), a component holds a control character or a
`:` (a reserved delimiter, so a rendered path always re-parses to the same
value), or `..` climbs above a view or alias root (`EscapesRoot`). A leading
`..` in a *relative* path is preserved for the caller to resolve.

The crate is `no_std` + `alloc` and `#![forbid(unsafe_code)]`. Parsing is the
only fallible step and never panics; it runs in time linear in the input with no
recursion, so neither a hostile path nor a hostile component can drive runaway
work. The `fuzz_path` harness checks that any byte string either parses or fails
closed and that every parsed path's canonical spelling re-parses to an equal
value.
