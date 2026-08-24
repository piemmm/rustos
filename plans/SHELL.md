# elsh (Element Shell) — the TAIRiX shell (`userland/shell/elsh`)

**elsh** ("Element Shell") is the default TAIRiX command interpreter; its
crate is `tairix-elsh` and its `Run` binary is `tairix-elsh-run`. It should
feel familiar to users of zsh/POSIX shells while adding structured output
discovery, predictable rendering, and the TAIRiX standard information stream
(`stdinfo`, fd 3). Where this document says "the shell" it means elsh.

The shell has two equally important jobs:

1. Run ordinary shell commands with zsh-style syntax and Unix-like stream
   semantics.
2. Let users and tools discover the shape, fields, views, and renderings of a
   command's output without turning every pipeline into verbose object text.

This document is normative unless a section is explicitly marked as an
implementation note.

## Companion specifications

This document was written before the resource-alias, storage-namespace, and
terminal-stack designs existed. It defers to them and MUST stay consistent
with them; where this document and one of them disagree, the owning document
below wins for the area it owns, and `AGENTS.md` wins over all of them.

- **Resource aliases and selector namespaces** (`plans/ALIAS.md`). Owns the
  typed, non-filesystem resource reference grammar
  `namespace:selector[@guard][::facet][?params]` and the namespace registry
  (`sys:`, `info:`, `stats:`, `state:`, `disk:`, `part:`, `vol:`, `tty:`,
  `net:`, `cap:`, …). The shell consumes resource references in redirection
  targets, command arguments, completion, and typed shell values; it never
  defines its own parallel namespace registry or a second reference parser.
- **Drives, volumes, aliases, and path namespace** (`plans/DRIVES.md`). Owns
  filesystem path spelling: storage is a forest of named roots, addressed by
  the user shorthand `Alias:/path` (internal `alias::Alias/path`) and the
  stable `id::<volume-id>/path`, with `/` retained only as a synthetic session
  compatibility view. The shell consumes alias paths in `cd`, prompt display,
  word expansion, and completion through the single shared path parser; it
  never hard-codes a fixed top-level directory set or a second path parser.
- **Terminal vocabulary, termcap, and curses** (`plans/CURSES.md`). Owns the
  shared ANSI/VT/xterm escape vocabulary (`lib/vt`), the compiled-in
  capability database (`lib/termcap`), and the curses/TUI screen model
  (`lib/curses`). The shell's interactive rendering, key decoding, and any
  full-screen affordance go through that one vocabulary, never a second
  divergent escape-sequence definition (`AGENTS.md` §2.2). See "Interactive
  terminal" for how the REPL line editor, capability-keyed rendering, and
  completion-menu display build on this stack.

A note on the word **alias**. This document already used "alias" for an
ordinary shell *command* alias (`alias ll='ls -l'`), expanded by the lexer.
That meaning is unchanged. It is distinct from a **resource alias**
(`ALIAS.md`, e.g. `disk:backup`) and a **path/root alias** (`DRIVES.md`, e.g.
`Home:`). Where ambiguity is possible this document says "command alias",
"resource alias", or "path alias" explicitly.

## Terminology

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as implementation requirements.

- **Primary data** means bytes written to `stdout` (fd 1).
- **Diagnostics** means errors and warnings written to `stderr` (fd 2).
- **Advisory information** means optional JSONL records written to `stdinfo`
  (fd 3).
- **Native stream** means a TAIRiX-aware structured stream carried on `stdout`.
- **Legacy stream** means ordinary byte/text output carried on `stdout`.
- **View** means a named presentation of a richer value, such as `names`,
  `long`, or `blocks-long` for `ls`.
- **Render** means converting values to text or bytes at a boundary.
- **Save** means preserving structured values in a structured file format.

## Design rules

1. **zsh-like command lines remain boring.** A user typing `ls >mylisting.txt`
   gets a simple text file, not a dump of internal objects.
2. **`stdout` is always primary.** fd 1 is the only data stream piped by `|`.
3. **`stdinfo` is optional and ignorable.** fd 3 may help humans, completions,
   AI tools, and introspection utilities, but it MUST NOT affect correctness,
   security, exit status, or ordinary pipeline semantics.
4. **Structure is preserved between native tools when explicitly negotiated.**
   Native pipes MUST use compact framing and schema metadata, not repeated
   `{ field=value }` text for every row.
5. **Text remains the universal fallback.** Any native command MUST be able to
   render stable text for files, terminals, and legacy commands.
6. **Discovery is first-class.** `schema`, `fields`, `headers`, and `views`
   are required userland utilities, with shell support for convenient pipeline
   and tab-completion workflows.
7. **No ambient authority.** The interpreter decides what to run; injected
   seams perform process, stream, directory, and completion operations.
8. **Failure closes safely.** Unsupported syntax, incomplete probes, malformed
   metadata, and invalid schemas must fail predictably and without partial
   execution of a line that did not parse.

## Crate and implementation constraints

The shell crate is `no_std` with `alloc`. Production paths MUST contain no
`unsafe`, no `unwrap`, no `expect`, and no `panic!`.

The shell depends only on audited TAIRiX interface crates such as `lib/abi`.
A userland program MUST NOT link kernel or driver crates.

## Pure interpreter architecture

The shell decides what to run, how to expand words, how to connect streams,
and how to apply control-flow operators. It does not itself name devices,
open terminals, talk to UARTs, or call kernel facilities directly. Like every
TAIRiX text program, the shell performs all of its own text I/O over the four
inherited standard streams (fd 0/1/2/3) and never over a kernel-discovered
console, UART, or framebuffer device (`AGENTS.md` §20).

External effects are reached through injected seams:

- `ProcessHost`: launches, waits for, signals, and polls jobs; changes the
  shell working directory; exposes stable `Errno` failures.
- `Console`: the REPL's view of the shell's own inherited standard streams.
  It reads interactive command-line input from `stdin` (fd 0), writes the
  prompt and REPL `stdout` text to `stdout` (fd 1), and writes REPL
  diagnostics to `stderr` (fd 2). It is never a console/UART device handle
  (`AGENTS.md` §20). Interactive line editing, prompt rendering, key/escape
  decoding, and any full-screen affordance are expressed through the shared
  terminal stack of `plans/CURSES.md` (`lib/vt` vocabulary, `lib/termcap`
  capabilities keyed off the inherited `TERM`, and `lib/curses` for screen
  models), never a second, shell-private escape-sequence vocabulary
  (`AGENTS.md` §2.2). An unknown or missing `TERM` degrades to a safe baseline
  rather than failing (`AGENTS.md` §2.9).
- `CompletionHost`: lists commands, paths, variables, jobs, command metadata,
  cached `stdinfo`, and command descriptors for tab completion.
- `InfoHost`: provides the shell-owned best-effort sink or capture path for
  fd 3 when no explicit `3>` redirection is present.

On a running kernel these seams are syscall-backed: the stream seams operate
on the shell's inherited standard-stream descriptors, never on a device the
kernel discovered (`AGENTS.md` §20). In tests they MUST be in-memory fixtures
so lexing, parsing, expansion, redirection, job control, `stdinfo`, and
completion can be tested without a kernel.

## Run pipeline

`Shell::run_line` is the entry point. For each line it MUST:

1. Report finished background jobs before the next prompt.
2. Lex the line using zsh-compatible quoting and escaping rules.
3. Parse the token stream into a command list joined by `;`, `&&`, `||`,
   newlines, and optional `&` background markers.
4. Expand words according to the configured compatibility level.
5. Resolve builtins, functions, aliases, and external commands.
6. Build an execution plan with stdin/stdout/stderr/stdinfo wiring.
7. Apply any introspection rewrites, such as `ls | schema`.
8. Run each entry whose connector condition is satisfied.
9. Set `$?` to the foreground status or the shell parse/usage status.

A line that fails lexing or parsing MUST run nothing and set `$?` to `2`.

## zsh-style compatibility target

The shell is not required to be a bug-for-bug zsh clone, but its surface syntax
SHOULD be compatible with zsh for common scripts and interactive use.

### Required command forms

The parser MUST support:

```sh
simple-command arg1 arg2
name=value other=value command arg
pipeline | next | final
pipeline |& next
cmd ; next
cmd && on-success
cmd || on-failure
cmd &
( list )
{ list; }
! pipeline
```

The parser SHOULD reserve room for zsh-style functions:

```sh
name() { list; }
function name { list; }
```

Unsupported compound forms MUST fail closed with a parse error before running
any command in the line.

### Quoting and comments

The lexer MUST support:

```sh
'literal text'
"interpolated $text"
\ escaped-character
# comment beginning at command-word boundary
```

The lexer SHOULD support zsh-style ANSI-C quoted strings:

```sh
$'line\ntext'
```

### Expansion target

The shell SHOULD converge on the following zsh-compatible expansion order:

1. Command-alias expansion (the lexer's `alias ll='ls -l'` substitution; not a
   resource or path alias — see "Companion specifications").
2. Brace expansion.
3. Tilde expansion. `~` and `~user` are a zsh convenience; the canonical home
   reference is the `Home:` path alias (`plans/DRIVES.md`). A bare `~` expands
   to the current user's home root, which the path layer maps to `Home:/`.
   Tilde expansion is resolved through the single shared path parser, never a
   second home-directory lookup.
4. Parameter expansion.
5. Command substitution.
6. Arithmetic expansion.
7. Filename generation/globbing.
8. Quote removal.

The minimum expansion set is:

```sh
$NAME
${NAME}
$?
$$
$!
$#
$1
${array[1]}
$(command)
`command`
$(( expression ))
```

The first implementation MAY support only `$NAME`, `${NAME}`, and `$?`, but it
MUST document each unsupported expansion and fail closed where continuing would
change command meaning.

### Redirection syntax

The shell MUST support ordinary zsh/POSIX-style redirections:

```sh
cmd <input
cmd >output
cmd >>output
cmd >|output
cmd 2>errors
cmd 2>>errors
cmd 2>&1
cmd 1>&2
cmd 3>info.jsonl
cmd 3>>info.jsonl
cmd 3>&-
cmd &>combined
cmd &>>combined
cmd <<EOF
cmd <<<word
```

The shell SHOULD support read-write redirection and fd duplication:

```sh
cmd <>file
cmd n>&m
cmd n<&m
```

### zsh-only redirection operators

The operators above include the redirections shared with POSIX. zsh adds the
following forms that POSIX does not standardize; the shell SHOULD support them
so that zsh scripts and interactive habits carry over. Each is additive and
never removes the meaning of a POSIX form.

Clobber-override and append synonyms (zsh spells `>|` two ways and extends it
to append):

```sh
cmd >!output          # synonym for >|output (clobber even with noclobber)
cmd >>|output         # append, overriding noclobber
cmd >>!output         # synonym for >>|output
```

csh/zsh combined stdout+stderr redirection (synonyms for the `&>` forms above),
including their clobber-override spellings:

```sh
cmd >&combined        # synonym for &>combined
cmd >>&combined       # synonym for &>>combined
cmd &>|combined       # combined, overriding noclobber
cmd &>!combined       # synonym for &>|combined
```

Here-document with leading-tab stripping, and here-strings on an explicit fd:

```sh
cmd <<-EOF            # here-doc; leading tabs on body and terminator stripped
cmd 0<<<word          # here-string on an explicit fd number
```

Dynamic file-descriptor allocation: `{var}` to the left of a redirection
operator allocates an unused fd (≥ 10), performs the redirection on it, and
binds the chosen number to the shell parameter `var`:

```sh
cmd {fd}>output       # allocate an fd, open `output` on it, set $fd
cmd {fd}>&-           # close the previously allocated fd
```

The shell MUST NOT allocate fd 0, 1, 2, or 3 for a `{var}` redirection; those
are the reserved standard streams (`stdin`, `stdout`, `stderr`, `stdinfo`).

`>|`, `>!`, `>>|`, `>>!`, `&>|`, and `&>!` MUST clobber even when noclobber is
active. `>`, `>>`, `&>`, `&>>`, `>&`, and `>>&` SHOULD honor the shell's
noclobber option if that option is implemented.

### Multios

zsh "multios" attach more than one target to a single stream. The shell
SHOULD support them:

```sh
cmd >out.txt >copy.txt    # write stdout to both files (tee-like)
cmd <part1 <part2         # read stdin as the concatenation of both files
```

When multios are enabled, repeating an output redirection for the same fd
fans the stream out to every target; repeating an input redirection feeds the
inputs in order. A multios target list MAY mix ordinary paths with the
redirection target namespaces above (`cmd >log.txt >sys:null`); each target is
resolved independently by the §"Resolution rule" test. Multios MUST fail
closed if any target fails to open, and MUST NOT partially apply a redirection
(`AGENTS.md` §5.4).

### Process substitution

zsh process substitution exposes a command's stream as a path-like argument.
The shell SHOULD support the read and write forms:

```sh
diff <(cmd-a) <(cmd-b)    # each <(...) is a readable stream of that command
cmd > >(consumer)         # >(...) is a writable stream into that command
```

The substituted token resolves to a kernel stream backing (`AGENTS.md` §20),
not a `/dev/fd`- or `/proc`-style path — TAIRiX has neither (`AGENTS.md`
§16.1). The named-pipe/anonymous-fd plumbing is owned by the shell and the
kernel IPC layer; no temporary filesystem entry is created. zsh's `=(...)`
temporary-file form is **not** supported, because it would require a writable
scratch path and TAIRiX has no `/tmp` (`AGENTS.md` §16.1); the shell MUST
report `=(...)` as an unsupported expansion and fail closed rather than invent
a scratch location.

fd 3 is named `stdinfo`. It remains an ordinary file descriptor for
redirection syntax, but it is reserved by convention and ABI for advisory
information about the command's `stdout` or operation.

## Redirection target namespaces

TAIRiX has no `/dev`, `/proc`, `/sys`, or `/etc`, and storage is not one
fixed Unix tree (`AGENTS.md` §16.1; `plans/DRIVES.md`). The Unix idiom of
redirecting to or from a device file (`> /dev/null`, `< /dev/zero`,
`< /dev/random`) therefore cannot be a filesystem path in TAIRiX. Those
sinks and sources, and every other non-filesystem stream a redirection can
name, are **resource references** owned by `plans/ALIAS.md`: a typed
reference of the form `namespace:selector[@guard][::facet][?params]` that the
shell resolves to a kernel **stream backing** (`AGENTS.md` §20) instead of a
path.

The shell does not define its own namespace registry. The recognised
namespaces (`sys:`, `tty:`, `disk:`, `vol:`, …) and their selector grammar
live in `ALIAS.md`; the parser is the single shared resource-reference parser
(`ALIAS.md` §16.1), never a second shell-private one (`AGENTS.md` §2.2). A
redirection MAY name a resource reference **only when that resource exposes a
stream facet** (`ALIAS.md` §15.3); resolving a non-stream resource (e.g. an
`info:`/`stats:` answer) in target position MUST fail closed.

### Syntax

A redirection target whose prefix before `:` is a registered resource
namespace is resolved as a stream backing, not a filesystem path:

```sh
ls  > sys:null            # discard stdout
cmd < sys:zero            # read an endless run of 0x00
cmd < sys:random          # read CSPRNG bytes (AGENTS.md §22)
ls  > sys:full            # writes fail closed with NoSpace
tty monitor tty:debug > log.txt   # a tty resource's stream facet
```

`sys:` carries the well-known byte streams below; the broader namespace set
and its identity guards (`@fingerprint`), facets (`::raw`), and query
parameters (`?…`) are defined in `ALIAS.md`. The resolution rule below is
written for *whatever* namespace is registered; only a registered namespace
is ever special.

### Resolution rule (resource reference vs. path alias vs. filesystem path)

The shell MUST apply this test before any VFS lookup. A target is resolved
as a resource reference **only** when every clause holds; otherwise it is a
path (a `DRIVES.md` alias path or an ordinary filesystem path):

```
if the target is a relative path
and its first path component contains ':'
and the substring before that ':' is a registered resource namespace
and the character immediately after that ':' is not '/'
and that prefix is neither "." nor ".."
then resolve the target as a resource-reference stream backing
else resolve the target as a path (alias path or filesystem path)
```

The "character after `:` is not `/`" clause keeps the two `:` worlds apart:
a `DRIVES.md` path alias is always written `Alias:/path` (the `:` is
immediately followed by `/`), so it resolves as a path, while a resource
reference's selector never begins with `/`. The consequence is that every
real file stays addressable on every mounted root — nothing on disk is
consulted for a resource target, and a path escape always exists:

| target            | resolves to                                                  |
|-------------------|--------------------------------------------------------------|
| `sys:random`      | resource-reference stream backing (registered namespace)     |
| `Home:/notes`     | path alias (prefix followed by `/`, `plans/DRIVES.md`)       |
| `/sys:random`     | absolute filesystem path (not a relative path)               |
| `./sys:random`    | filesystem path (first component is `.`)                     |
| `foo/sys:random`  | filesystem path (first component `foo` has no `:`)           |
| `foo:bar`         | filesystem path (`foo` is not a registered namespace)        |

This matters because `:` is a legal filename byte on ext2/3/4 and POSIX
volumes (only `NUL` and `/` are forbidden), so no printable sigil can be
"reserved" on disk. The rule reserves nothing on any filesystem; it only
gives a registered namespace a meaning in *target position*, and a real file
named `sys:random` is always reachable as `./sys:random` or when quoted.

### Well-known streams

The `sys:` byte-stream targets resolve to a **closed** set of stream backings
defined once in `lib/abi` as a versioned, hashed, frozen-on-release enum
(`AGENTS.md` §9, §23.2) — never a string-keyed device table:

| target        | read behavior         | write behavior        |
|---------------|-----------------------|-----------------------|
| `sys:null`    | immediate EOF         | bytes discarded       |
| `sys:zero`    | endless `0x00`        | bytes discarded       |
| `sys:full`    | endless `0x00`        | fails with `NoSpace`  |
| `sys:random`  | CSPRNG bytes (§22)    | bytes discarded       |

`sys:random` MUST draw from the single kernel random subsystem behind
`tairix_abi::random` (`AGENTS.md` §22). It MUST NOT introduce a second
entropy source, PRNG, or seeding path.

### Capabilities and failure

Resolving a resource reference is a capability-checked operation that returns
a typed capability/descriptor, never a bare pathname (`ALIAS.md` §3.11,
§4); the shell supplies the redirection's direction as the resolve intent
(`Read` for `<`, `Write` for `>`/`>>`). The `sys:` byte streams need no
special capability beyond the §22 checks for `random`; other namespaces carry
their own capability and identity requirements (`ALIAS.md` §6). A destructive
target in non-interactive execution MUST carry an identity guard
(`ALIAS.md` §6.5); the shell MUST NOT silently resolve an unguarded
destructive resource. There is no ambient "open any device" surface
(`AGENTS.md` §4).

Resolution MUST fail closed (`AGENTS.md` §5.4):

- A target with a **registered** namespace prefix but an **unknown or
  non-stream** selector/facet (e.g. `sys:nul`, or a non-stream `info:` answer
  in target position) MUST be a hard error. The shell MUST NOT fall back to
  creating a file, so a typo can never silently produce junk on disk.
- An identity guard that does not match, is ambiguous, or is stale MUST fail
  with the `ALIAS.md` diagnostic rather than resolving (`ALIAS.md` §9.3).
- An **unregistered** prefix is not special: it is a path, exactly as the
  resolution rule states.

### Typed resource values

Where the shell exposes a resource selection as a shell value — for example
binding the result of a selector to a variable — that value MUST be a **typed
resource value**, not a plain string (`plans/ALIAS.md` §15.2):

```sh
let target = pick disk:?removable=true,size>=16GiB
image-write installer.img -> $target::raw
```

A typed resource value carries the resolved identity, not just a name:
`resource_kind`, `canonical_identity`, `short_fingerprint`, `facet_rights`,
`generation`, and `scope` (`plans/ALIAS.md` §15.2). The shell MUST resolve the
selection through the single shared resolver (it never re-implements
resolution in-process) and MUST re-check the generation/identity at use time so
a stale handle fails closed rather than acting on the wrong device
(`plans/ALIAS.md` §9.3, §3.11).

Serializing a typed resource value back to text (for display, logging, or
`stdout`) MUST serialize only its identity, never its authority: the value's
underlying capability/descriptor is not embedded in, or reconstructable from,
the text form (`plans/ALIAS.md` §15.2; `AGENTS.md` §4, §5.2). A text rendering
of `$target` is therefore safe to print and pipe, and confers no access.

Typed resource values are an optional shell feature; a shell without variables
need not provide them, but a shell that does provide them MUST follow the rules
above rather than store resource selections as bare strings.

## Standard streams

Every foreground command receives these inherited standard streams unless
redirections override them:

| fd | name       | purpose                                      |
|----|------------|----------------------------------------------|
| 0  | `stdin`    | primary input                                |
| 1  | `stdout`   | primary output data                          |
| 2  | `stderr`   | errors and diagnostics                       |
| 3  | `stdinfo`  | optional structured advisory JSONL metadata  |

### `stdout`

`stdout` is the primary data stream. `cmd | next` pipes only fd 1. `cmd >file`
redirects only fd 1. The shell MUST NOT silently replace primary data with
metadata.

### `stderr`

`stderr` is for diagnostics. `|&` follows zsh convention and pipes fd 1 and fd
2 to the next command's stdin. `|&` MUST NOT include fd 3.

### `stdinfo`

fd 3 is `stdinfo`. It is a reserved, optional, structured advisory stream.
Commands MAY write JSONL `StdInfoRecord` values to fd 3. With no consumer
attached, writes to fd 3 MUST be best-effort and non-blocking.

Writing to fd 3 MUST NOT change correctness, security, exit status, scripting
semantics, or ordinary pipeline behavior. A command that cannot write
`stdinfo` because the fd is closed, redirected, full, or unsupported MUST
continue as though no `stdinfo` consumer exists.

Security events MUST go through the TAIRiX logging facility, not fd 3. AI and
automation consumers MUST treat `stdinfo` as untrusted data about a command,
never as authority, policy, or instructions.

`cmd | next` pipes only fd 1:

```sh
cmd | next
```

`cmd 3>info.jsonl` captures advisory metadata:

```sh
cmd >out.txt 3>info.jsonl
```

`cmd 3>&-` closes the advisory stream:

```sh
cmd 3>&-
```

The shell MAY provide a named redirection alias in documentation and display,
but the concrete syntax remains zsh-compatible fd redirection:

```sh
cmd 3>info.jsonl      # capture stdinfo
```

## `stdinfo` records

Each `stdinfo` record is one JSONL line carrying:

- `version`
- `producer`
- `kind`
- stable machine `code`
- `severity`, which MUST be `info` or `debug`; security-relevant events go
  through the TAIRiX logging facility, never fd 3 (`AGENTS.md` §20.1)
- terse human message with at most one suggestion
- producer-supplied structured `ai` object

The record kind set is closed. The only valid kinds are:

| kind         | meaning                                                       |
|--------------|---------------------------------------------------------------|
| `omission`   | output was hidden, skipped, filtered, truncated, or not shown |
| `summary`    | a short, non-obvious result summary                           |
| `schema`     | stdout structure, columns, units, or encoding                 |
| `suggestion` | a safe optional next action; never auto-run                   |
| `context`    | concise environmental context needed to interpret stdout      |

Producers MUST NOT invent synonymous kinds such as `hint`, `tip`, `notice`,
`info`, `advice`, `warning-lite`, `metadata`, or `metadata-note`.

For structured output discovery, producers SHOULD emit a `schema` record when
one or more of the following is true:

- `stdout` is a native stream.
- `stdout` is a table, record stream, CSV, JSONL, or other structured text.
- The command supports named views.
- The command hides, truncates, filters, or omits output that a human or tool
  may need to know about.
- The command's terminal view differs from its redirected/file view.

A `schema` record's `ai` object SHOULD include:

```json
{
  "stream": "stdout",
  "kind": "record-stream",
  "type": "stream<FileEntry>",
  "schema_id": "tairix.fs.FileEntry@1",
  "default_view": {
    "terminal": "grid-names",
    "redirect": "names",
    "legacy_pipe": "names",
    "native_pipe": "native"
  },
  "fields": [
    { "name": "name", "type": "string", "nullable": false, "description": "base filename" },
    { "name": "size", "type": "size", "nullable": true, "unit": "bytes" }
  ],
  "views": [
    { "name": "names", "fields": ["name"], "header": false },
    { "name": "long", "fields": ["permissions", "links", "owner", "group", "size", "modified", "name"], "header": false }
  ]
}
```

The exact ABI field names for the surrounding `StdInfoRecord` are defined by
`tairix_abi::stdinfo`. The `ai` object is producer-defined but SHOULD follow
the shape above for schema-aware tooling.

## Output model

Commands write primary data to `stdout`. A command's `stdout` may be one of:

| kind             | description                                      |
|------------------|--------------------------------------------------|
| `bytes`          | opaque bytes                                     |
| `text`           | text without line structure guarantees           |
| `lines`          | text lines                                       |
| `table`          | rows and columns                                 |
| `record-stream`  | typed records, possibly open or schema inferred  |
| `event-stream`   | time-ordered records/events                      |
| `native`         | compact TAIRiX structured stream                 |

The shell MUST distinguish internal/native representation from rendered text.
Native representation is for native-aware commands. Rendered text is for
terminals, files, and legacy commands.

### Boundary defaults

The shell MUST use these default rules:

| context                       | default behavior                                  |
|-------------------------------|---------------------------------------------------|
| command writes to terminal    | render the command's terminal view                |
| command redirects with `>`    | render the command's stable redirected text view   |
| command pipes to legacy tool  | render the command's stable legacy pipe view       |
| command pipes to native tool  | MAY negotiate compact native stream               |
| command pipes to `render`     | use the explicitly requested renderer             |
| command pipes to `save`       | preserve structure in a structured file format    |

A command MUST NOT emit native binary/framed data to a terminal, ordinary file
redirection, or legacy command unless the user explicitly requested that format.

### Rendering and saving

Rendering converts values to text/bytes:

```sh
ls | render lines >names.txt
ls | render table --header >files.txt
ls | render table --no-header >files.txt
ls | render csv --header >files.csv
ls | render jsonl >files.jsonl
ls | render nul >names.nul
```

Saving preserves structured values:

```sh
ls | save files.rustdata
```

`render` and `save` SHOULD be ordinary userland programs. The shell MAY
optimize them when it can do so without changing observable behavior.

### Headered and headerless output

Headers are a rendering property, not a data property.

```sh
ls | select name,size,modified | render table --header >files.txt
```

writes a headered text table.

```sh
ls | select name,size,modified | render table --no-header >files.txt
```

writes the same selected fields without labels.

The native stream still has field names either way.

### Object text is never the implicit pipe format

Native record streams MUST be compact. They SHOULD send schema once, then
field-id/value batches. They MUST NOT repeat textual object notation for every
row unless the user explicitly requests a text serialization such as JSONL,
YAML, or debug format.

This is wrong as an implicit pipe format:

```text
{ name="foo.txt", size=1234, owner="ian" }
{ name="bar.txt", size=5678, owner="ian" }
```

This is acceptable only after an explicit request:

```sh
ls | render jsonl
ls | render debug
```

## `ls` output contract

`ls` is the reference command for the structured output model.

### Native type

`ls` produces a logical stream of `FileEntry` records.

Required fields:

| field         | type        | nullable | meaning                                      |
|---------------|-------------|----------|----------------------------------------------|
| `name`        | `string`    | no       | base filename                                |
| `path`        | `path`      | no       | path relative to invocation or full path      |
| `type`        | `enum`      | no       | file, directory, symlink, socket, fifo, etc. |
| `size`        | `size`      | yes      | byte size when meaningful/available          |
| `blocks`      | `integer`   | yes      | allocated filesystem blocks                  |
| `permissions` | `mode`      | yes      | permission bits                              |
| `links`       | `integer`   | yes      | hard-link count                              |
| `owner`       | `user`      | yes      | owning user                                  |
| `group`       | `group`     | yes      | owning group                                 |
| `modified`    | `datetime`  | yes      | modification time                            |
| `created`     | `datetime`  | yes      | creation/birth time when available           |
| `accessed`    | `datetime`  | yes      | access time                                  |
| `inode`       | `integer`   | yes      | inode number                                 |
| `device`      | `integer`   | yes      | device id                                    |
| `target`      | `path`      | yes      | symlink target                               |

### Required views

`ls` MUST provide these named views:

| view           | fields                                                                   | default header |
|----------------|--------------------------------------------------------------------------|----------------|
| `names`        | `name`                                                                   | no             |
| `grid-names`   | `name`                                                                   | no             |
| `long`         | `permissions links owner group size modified name`                       | no             |
| `blocks-long`  | `blocks permissions links owner group size modified name`                | no             |
| `table`        | `name type size modified`                                                | yes            |
| `full`         | all available fields                                                     | yes            |
| `debug`        | all available fields plus representation details                         | yes            |

### Required default behavior

Plain terminal use may be pretty:

```sh
ls
```

Redirected output MUST be stable and simple:

```sh
ls >mylisting.txt
```

The file MUST contain one rendered filename per line by default:

```text
bar.txt
foo.txt
notes.md
src
```

Long format remains available with familiar flags:

```sh
ls -l >mylisting.txt
```

Block count plus long format remains available with familiar flags:

```sh
ls -ls >mylisting.txt
```

The same behavior MUST also be expressible through views:

```sh
ls --view names >mylisting.txt
ls --view long >mylisting.txt
ls --view blocks-long >mylisting.txt
```

Field and renderer composition MUST be supported:

```sh
ls | select name,size,modified | render table --header >files.txt
ls | select name,size,modified | render table --no-header >files.txt
ls | select name | render lines >names.txt
ls | select name | render nul >names.nul
ls | render jsonl >files.jsonl
ls | save files.rustdata
```

Filenames containing newlines, tabs, escape bytes, or other awkward characters
MUST be escaped or encoded in human text views. Exact machine-safe filename
output SHOULD use `render nul`, `render jsonl`, or `save`.

## Required discovery utilities

The following small programs are required:

```sh
schema
fields
headers
views
```

They SHOULD be ordinary userland programs so they work in scripts, tests, and
non-interactive sessions. The shell MAY recognize and optimize them, but any
optimization MUST preserve their documented behavior.

The preferred non-streaming form is planner introspection:

```sh
schema  { pipeline; }
fields  { pipeline; }
headers { pipeline; }
views   { pipeline; }
```

Planner introspection MUST NOT run the inspected pipeline unless an explicit
option requests a runtime sample. It uses command descriptors, built-in
schemas, static transforms, and cached safe metadata.

The shell SHOULD also support pipeline shorthand:

```sh
ls | schema
ls | fields
ls | headers
ls | views
```

When one of the required discovery utilities appears as the final command in a
pipeline, the shell SHOULD interpret it as planner introspection of the
upstream pipeline rather than running the upstream command. This lets `ls |
headers` answer "what would this pipeline output?" without listing the
directory. To force ordinary streaming behavior, use the utility's explicit
stdin mode:

```sh
ls | command schema --stdin
ls | /Apps/Schema.app/Run --stdin
```

If planner introspection is impossible, the utility MUST say so clearly. It
MUST NOT silently execute a side-effecting pipeline to guess.

All four utilities MUST accept captured `stdinfo` JSONL:

```sh
cmd >out.txt 3>info.jsonl
schema  --from-stdinfo info.jsonl
fields  --from-stdinfo info.jsonl
headers --from-stdinfo info.jsonl
views   --from-stdinfo info.jsonl
```

All four utilities SHOULD accept these common options:

```sh
--format table|lines|json|jsonl
--header
--no-header
--stdin
--from-stdinfo PATH|- 
--strict
--quiet
```

### `schema`

`schema` prints the full available structure of a pipeline's output.

Example:

```sh
schema { ls; }
```

Output:

```text
type: stream<FileEntry>

field          type       nullable  description
name           string     no        base filename
path           path       no        path relative to invocation or full path
type           enum       no        file, directory, symlink, socket, fifo...
size           size       yes       byte size when meaningful/available
blocks         integer    yes       allocated filesystem blocks
permissions    mode       yes       permission bits
links          integer    yes       hard-link count
owner          user       yes       owning user
group          group      yes       owning group
modified       datetime   yes       modification time
created        datetime   yes       creation/birth time when available
accessed       datetime   yes       access time
inode          integer    yes       inode number
device         integer    yes       device id
target         path       yes       symlink target
```

For unstructured bytes:

```text
type: bytes
schema: none
```

For text lines:

```text
type: stream<line>
field:
  line: string
```

For inferred data, `schema` MUST report its uncertainty:

```text
type: stream<record>
schema certainty: inferred from 1000 rows
record openness: open
```

### `fields`

`fields` prints available field names only.

```sh
fields { ls; }
```

Output:

```text
name
path
type
size
blocks
permissions
links
owner
group
modified
created
accessed
inode
device
target
```

With `--format lines`, the output MUST be one field name per line. With
`--format json`, it MUST emit a JSON array of strings.

### `headers`

`headers` prints the columns or labels that the current view would render.

This differs from `schema`: `schema` describes all available data; `headers`
describes the visible presentation.

```sh
headers { ls; }
```

Output:

```text
name
```

```sh
headers { ls -l; }
```

Output:

```text
permissions
links
owner
group
size
modified
name
```

```sh
headers { ls -ls; }
```

Output:

```text
blocks
permissions
links
owner
group
size
modified
name
```

If the current output is unstructured text or bytes, `headers` MUST say that no
headers exist and exit non-zero unless `--quiet` is supplied.

### `views`

`views` prints named presentations supported by a command or pipeline.

```sh
views { ls; }
```

Output:

```text
view          fields
names         name
grid-names    name
long          permissions links owner group size modified name
blocks-long   blocks permissions links owner group size modified name
table         name type size modified
full          all fields
debug         all fields plus representation details
```

With `--format json`, `views` MUST include field names, header default,
terminal suitability, redirected-file suitability, and whether the view is
stable for scripts.

## Recommended companion utilities

The following utilities are strongly recommended because they make structured
pipelines useful without putting formatting policy into every producer:

```sh
describe
sample
select
where
sort-by
group-by
count
rename
render
save
parse
```

### `describe`

`describe` inspects a command without running it:

```sh
describe ls
describe render
describe schema
```

It SHOULD show command purpose, accepted options, input kinds, output kind,
fields, views, render defaults, side-effect profile, and completion metadata.

### `sample`

`sample` may run a command only when explicitly requested:

```sh
sample 5 { ls; }
```

It SHOULD show a few values plus schema. It MUST be clear that it executed the
pipeline.

## Planner introspection

The shell needs a planner capable of asking, "What would this pipeline output?"
without executing the pipeline.

A command descriptor SHOULD declare:

- command name and version
- input stream kinds accepted
- output stream kind
- schema id or schema inference rule
- fields and field types
- named views
- default terminal, redirect, legacy-pipe, and native-pipe views
- options that change schema or views
- whether completion probes are safe
- side-effect profile: read-only, writes-files, network, secrets, destructive,
  interactive, unknown

Planner introspection MAY combine transforms:

```sh
schema { ls | select name,size | sort-by size; }
```

Output:

```text
type: stream<Record>
fields:
  name: string
  size: size?
```

If a transform cannot be understood statically, the planner MUST report the
unknown boundary:

```text
schema: unknown after command `legacy-tool`
known before boundary: stream<FileEntry>
known after boundary: text
```

It MUST NOT invent fields.

## Paths, roots, and the current directory

TAIRiX storage is a **forest of named roots**, not one fixed Unix tree
(`plans/DRIVES.md`). The shell treats paths accordingly, and it never
hard-codes a top-level directory set such as `/System`, `/Users`, `/Apps`,
`/Storage`; those are default *view entries* backed by the path aliases
`System:`, `Users:`, `Apps:`, `Storage:`, not a frozen list the shell knows.

### Accepted path forms

A path argument or path-position token MAY be any of:

```text
Home:/Documents/spec.md          # user shorthand: an alias path (DRIVES.md)
id::<volume-id>/snapshots/2026    # stable canonical root + inner path
/Users/ian/Documents             # synthetic session view path (compatibility)
../src                           # relative path inside the current root
Documents/report.md              # relative path inside the current directory
```

All path forms are parsed by the **single shared path parser** (`DRIVES.md`
§16) used by the kernel, drivers, GUI, and shell alike; the shell MUST NOT
add a second path parser (`AGENTS.md` §2.2). Parsing a name into an object is
only a name-to-object step: every open/read/write still enforces inode
permissions, ACLs, capability gates, mount flags, and MAC policy
(`AGENTS.md` §5.3).

### `cd` and the current directory

`cd` accepts alias paths, the synthetic view path, and relative paths:

```sh
cd Home:/Documents
cd System:/Kernel
cd /Users/ian/Documents      # synthetic view path
cd ../src                    # relative to current root
```

A malformed selector such as `cd Home:Documents` (alias without `:/`) or
`cd C:Users` (drive-letter style) MUST fail closed, not guess. There is no
per-drive current directory: the shell has exactly **one** current directory,
held as a root handle plus a directory handle (`DRIVES.md` §17.2). A relative
path resolves inside the current root; it can never silently cross to another
root.

The current directory MUST survive the synthetic `/` view being absent,
hidden, or unhealthy: a process holding a root + directory handle keeps
working even when no `/` view or `Storage:` catalog is present
(`DRIVES.md` §3.2).

### Prompt display

The prompt SHOULD display the current location as an alias path when an alias
maps to the current root, falling back to the stable ID otherwise
(`DRIVES.md` §17.1):

```text
Home:/Projects/TAIRiX>
System:/Kernel>
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/>
```

The prompt is REPL `stdout` text rendered through the shared terminal stack
(see the `Console` seam); it is never written to a discovered console device
(`AGENTS.md` §20).

## Interactive terminal

The interactive REPL — its line editor, prompt rendering, key and escape
decoding, history navigation, and any menu or full-screen affordance — is
expressed entirely through the shared terminal stack of `plans/CURSES.md`:
the ANSI/VT/xterm vocabulary (`lib/vt`), the compiled-in capability database
(`lib/termcap`), and the curses/TUI screen model (`lib/curses`). The shell
MUST NOT define a second, shell-private escape-sequence vocabulary, key table,
or screen model (`AGENTS.md` §2.2). `lib/curses` is the OS-provided,
dynamically linked Terminal/TUI library (`AGENTS.md` §16.4); the shell links
it like any other curated `/System/Libraries/` library, so one fix to the
library covers every consumer (`plans/CURSES.md` §2).

All terminal I/O still flows over the shell's inherited standard streams
(fd 0/1/2). The terminal stack is I/O-injected over those descriptors through
the `Console` seam; it never opens a console, UART, or framebuffer device
(`AGENTS.md` §20). fd 3 (`stdinfo`) is reserved and advisory: the line editor
and curses screen model MUST NOT write to fd 3 (`plans/CURSES.md`, Stage C4).

### Capability-keyed rendering and input

The shell reads the terminal's capabilities from `lib/termcap`, keyed off the
inherited `TERM` value, and renders only what the active terminal supports:

1. Output attributes and colour MUST degrade by capability rather than emit
   sequences the terminal does not understand — truecolour → 256-colour →
   16-colour → monochrome — using `lib/curses`'s minimal-diff renderer, never
   hand-rolled cursor arithmetic (`plans/CURSES.md`, Stages C3–C4).
2. Key input — function, arrow, and editing keys, bracketed paste, and (where
   advertised) mouse reporting — MUST be decoded through `lib/vt`'s parser and
   `lib/termcap`'s key tables into typed key/mouse events, never a second
   shell-private input parser (`plans/CURSES.md`, Stage C4).
3. Terminal resize MUST be handled through the curses screen model; the prompt,
   line editor, and any open menu re-lay out at the new size.

### Fail-closed and non-interactive degradation

An unknown or missing `TERM` MUST degrade to the safe baseline (`dumb`, then a
`vt100`-class fallback) rather than fail; the shell MUST NOT panic and MUST NOT
read any file derived from `TERM` — there is no `/etc`/`terminfo` to read
(`AGENTS.md` §2.9, §16.1; `plans/CURSES.md` §0). When `stdin`/`stdout` is not
an interactive terminal (a pipe, a file, or `TERM=dumb`), interactive line
editing, the completion menu, and full-screen affordances MUST degrade to
plain line input and plain `stdout` text; scripted and piped execution MUST
behave identically with or without a capable terminal.

### Completion menu rendering

The completion *result model* and contexts are defined under "Tab expansion
and completion". Their on-screen presentation — the candidate menu, resource
cards (`plans/ALIAS.md` §15.1), annotations, and stale markers — is drawn
through `lib/curses` over the same capability-keyed path above. On a terminal
that cannot support a menu the shell MUST fall back to a plain inline listing.

### Remote terminals

When the shell runs over a remote serial or SSH session, the terminal the user
sees is the terminal application's concern, not the shell's (`plans/CURSES.md`,
Stage C6). The shell only consumes the `TERM`/capabilities it inherits and
emits sequences through the one shared `lib/vt` vocabulary, so the same shell
output is correct locally and across a remote link end-to-end (`AGENTS.md`
§2.2).

## Tab expansion and completion

Tab expansion is a required feature of the interactive shell. It SHOULD feel
zsh-like but use schema metadata where available.

### Completion principles

1. Completion MUST NOT change `$?`.
2. Completion MUST NOT run side-effecting commands.
3. Completion MUST NOT write files, open network connections, request secrets,
   or perform destructive operations.
4. Completion MAY use static command descriptors, builtins, cached `stdinfo`,
   command metadata, and safe probes declared by command descriptors.
5. Completion MUST treat `stdinfo` as untrusted hints.
6. Completion MUST mark inferred, stale, partial, and unknown schemas.
7. Completion MUST degrade gracefully to path/word completion when schema is
   unavailable.
8. Completion of a resource reference (`plans/ALIAS.md`) MUST be
   namespace-aware and command-intent-aware: for a destructive command it
   MUST insert an identity-guarded reference (e.g. `format disk:backup@7K2M`),
   and it MUST NOT hide an identity mismatch (`ALIAS.md` §15.1). Resolving a
   guard or listing candidates for display is a read-only step that obeys
   principles 1–3.

### Completion result model

A completion candidate SHOULD carry:

```text
insert_text       text inserted into the command line
display_text      text shown in the menu
annotation        short type/kind label
description       one-line explanation
replacement_span  byte range to replace
suffix            optional suffix such as space, slash, comma, or equals
priority          ordering hint
source            path, command, builtin, schema, stdinfo-cache, descriptor
confidence        exact, inferred, stale, unknown
```

The display text may be rich; the inserted text MUST be minimal and valid.

Every candidate MUST be text the shell is willing to insert: there is no
display-only class. Where the registry names only the *shape* of what comes
next — a resource-selector placeholder such as `<iface>` — completion MUST
either offer the real names behind it or offer nothing there; it MUST NOT show
the placeholder spelling, which the shell could not insert and the user could
not use.

### Required completion contexts

At command position:

```sh
<TAB>
```

Suggest builtins, functions, command aliases, external commands, and
executable paths.

After a command option prefix:

```sh
ls --<TAB>
```

Suggest options from descriptors and `describe` metadata.

After a view option:

```sh
ls --view <TAB>
```

Suggest available views:

```text
names         stable redirected filename list
grid-names    terminal-friendly filename grid
long          permissions links owner group size modified name
blocks-long   blocks plus long columns
table         name type size modified
full          all fields
debug         representation details
```

After a field selector:

```sh
ls | select <TAB>
```

Suggest fields:

```text
name          string     base filename
path          path       path relative to invocation or full path
type          enum       file, directory, symlink...
size          size?      byte size when meaningful/available
blocks        integer?   allocated filesystem blocks
permissions   mode?      permission bits
modified      datetime?  modification time
```

After a comma in a selector:

```sh
ls | select name,<TAB>
```

Suggest remaining fields and avoid already-selected fields unless explicitly
requested.

Inside a predicate:

```sh
ls | where <TAB>
```

Suggest field-aware expression templates:

```text
name contains "..."
type == file
type == directory
size > 1MiB
modified before 2026-01-01
owner == "..."
```

After a comparison operator:

```sh
ls | where type == <TAB>
```

Suggest enum values when known:

```text
file
directory
symlink
socket
fifo
```

After a renderer:

```sh
ls | render <TAB>
```

Suggest render formats:

```text
lines
table
csv
tsv
json
jsonl
yaml
nul
debug
```

After header options:

```sh
ls | render table --<TAB>
```

Suggest renderer-specific options such as `--header`, `--no-header`,
`--width`, and `--escape`.

For discovery utilities:

```sh
schema --<TAB>
fields --<TAB>
headers --<TAB>
views --<TAB>
```

Suggest common options:

```text
--format
--header
--no-header
--stdin
--from-stdinfo
--strict
--quiet
```

For redirections:

```sh
cmd > <TAB>
cmd 2> <TAB>
cmd 3> <TAB>
```

Suggest paths, alias paths (`Home:/`, `System:/`, …), and — where a stream
facet applies — registered resource namespaces (`sys:null`, `tty:debug`, …).

A **writing** redirection (`>`, `>>`, `<>`, `&>`) MUST offer only
**stream-backed** namespaces: `info:`, `state:`, and `stats:` are typed values
read through the System Information API broker and changed by typed service
commands, so no write could ever open one and offering their namespace
prefixes or selectors there would lead the user into a dead end (`ALIAS.md`
§15.3). The refusal is the kernel's: a `resource_open` of a value-backed
namespace fails with `Errno::NotSupported` ("this backing cannot represent the
request"), distinct from the `Errno::NotImplemented` of a stream namespace
whose resolver has not landed.

A **reading** redirection (`<`, `n<`) MUST offer them, because the shell
serves such a read itself. `cat < info:mem/physical` lowers to a *value pipe*:
the shell resolves the reference through the one userspace resolver
(`lib/procinfo::valueread`) under its own kernel-attested identity, writes the
rendered value into a pipe, and wires the read end to the child's descriptor.
The child sees an ordinary stream and needs no `CAP_SYSINFO_*` of its own,
which is what makes every stdin-consuming tool able to read a fact without
each bundle requesting the authority. The read is resolved in the executor's
*open* phase, before any member spawns, so a refusal aborts the whole launch
with the capability named — a denied read can never reach the child as an
empty stream that reads like an empty value.

For `3>`, the completion menu SHOULD annotate the target as a `stdinfo` JSONL
capture and MAY prefer `.jsonl` names, but it MUST still allow any valid path.

For a resource reference (`plans/ALIAS.md`):

```sh
cmd < sys:<TAB>
disk info disk:<TAB>
format disk:backup<TAB>
```

Within a namespace, suggest the namespace's selectors and, for destructive
intents, complete to a guarded reference and show resource cards rather than
bare names (`ALIAS.md` §15.1):

```text
disk:backup@7K2M      Samsung SSD 870 EVO   4 TiB   pinned, non-removable
disk:installer@P91Q   SanDisk Ultra USB     32 GiB  removable, empty
```

A selector completes **one `/`-separated segment at a time**, exactly as a
path does, from the namespace registry's selector catalogue (`ALIAS.md` §15.1)
— never a completion-only table:

```text
state:<TAB>            irq/  net/
state:net/<TAB>        resolver/  wan  bond0
state:net/wan/<TAB>    active-member  address  link  member-health
```

A word the shared resolution rule reads as a resource reference MUST be
completed only as one, in every position including command position: it can
never denote a path, so path candidates for it would offer something the shell
would not open. Conversely a word that is not yet reference-shaped is offered
the registered namespace prefixes alongside its path candidates.

A catalogued segment the registry cannot enumerate — a placeholder such as
`<iface>` — names a typed *selector domain* (an interface, a bond, an
interrupt line, a CPU, a resource-limit kind, a reclaim class). Completion
MUST expand it into that domain's real names and offer those as ordinary
candidates, and completion resumes past the chosen name.

Enumeration is **capability-adaptive**, and that is the required behaviour
rather than a fallback. A domain drawn from a closed table another crate owns
(a resource-limit kind, a reclaim class) or from an ungated query (a CPU
index) MUST be offered to every session. A domain whose listing is gated —
an interface or interrupt line needs `CAP_SYSINFO_HW`, a bond alias needs
`CAP_SYSINFO_GLOBAL` — MUST be offered only to a session holding that
capability, and a session without it MUST be offered nothing there. The shell
MUST NOT be granted `CAP_SYSINFO_*` to make Tab richer: it is the most
exposed program on the machine, and a session that cannot list interfaces
could not read an interface's facts either, so it loses nothing. A gated
domain the session does not hold MUST be skipped **without issuing the
query**, so a Tab press never produces a denied request or an audit refusal
record.

The session's own capability set MUST be read through the ungated,
self-scoped process-identity query and MAY be cached for the process's life
(`elevate` spawns a program under another identity and never re-credentials
the shell). The *names* MUST NOT be cached across Tab presses, so a
hot-plugged interface appears immediately.

The selector **catalogue** itself is never filtered by capability: discovery
is not authorization and a spelling grants nothing (`ALIAS.md` §6.2), so
`info:mem/physical` is still completed for a session without
`CAP_SYSINFO_KERNEL` and the read then fails with an error naming the
capability. Only placeholders adapt, because a placeholder the shell cannot
expand is a lie.

Where a selector's reference is invalid without a query parameter — a
windowed rate — completion MUST insert that parameter rather than closing the
word as finished:

```text
stats:net/wan/rx.pp<TAB>    stats:net/wan/rx.pps?window=
```

For an alias path (`plans/DRIVES.md`):

```sh
cd <TAB>
cd Home:/<TAB>
```

Suggest the path aliases in scope and then entries inside the selected root.
Resource references and alias paths are completed through the same shared
parsers the shell uses to resolve them, never a second completion-only parser.

For file descriptors:

```sh
cmd 3>&<TAB>
```

Suggest open fds and `-` for close:

```text
0    stdin
1    stdout
2    stderr
3    stdinfo
-    close descriptor
```

### Completion and `stdinfo` cache

The interactive shell SHOULD keep a small cache of recent `stdinfo` `schema`
records keyed by:

- command path/name
- argv that affects output shape
- working directory where relevant
- environment variables declared shape-relevant by the descriptor
- command version if known

Cached data MUST be considered advisory and may be stale. Completion display
SHOULD mark stale entries.

Example:

```text
size    size?    byte size when available    [cached stdinfo]
```

### Safe probes

A descriptor may declare a probe command that is safe for completion, e.g.:

```text
completion.safe_probe = true
completion.probe_args = ["--describe-output"]
completion.no_network = true
completion.no_write = true
completion.no_secrets = true
```

The shell MAY run such probes for completion. It MUST enforce the declared
limits through capabilities when the OS supports them. If limits cannot be
enforced, the shell SHOULD prefer static descriptors and cached metadata.

## Native stream negotiation

Native stream negotiation is an optimization and ergonomics feature. It MUST
NOT break zsh-style behavior.

A command connected to a terminal, file redirection, or legacy command defaults
to rendered text. A command connected to a native-aware command MAY use a
compact native stdout representation if both sides agree.

Example:

```sh
ls | where size ">" 1MiB | select name,size
```

The shell MAY negotiate native record streams between `ls`, `where`, and
`select`. If the final output goes to a terminal, the last command renders a
human view. If it goes to `>file`, the last command renders its stable
redirected text view unless `save` or `render` says otherwise.

A native stdout stream SHOULD contain:

- stream magic/version
- schema id or inline schema
- field ids
- value batches
- end record and status metadata if needed

The same command SHOULD also emit a `stdinfo` `schema` record on fd 3 so
non-consuming tools, completion, and AI assistants can understand the stream.

If native negotiation fails, the shell MUST fall back to legacy text or fail
with a clear error. It MUST NOT pass binary native framing to a legacy command
that did not opt in.

## Legacy interoperation

Legacy commands are first-class. They consume and produce bytes/text.

```sh
ls | grep foo
```

Since `grep` is legacy unless described otherwise, `ls` MUST render its stable
legacy pipe view. That view is one filename per line by default.

Explicit legacy mode is available:

```sh
ls | render lines | grep foo
```

Explicit native preservation is available:

```sh
ls | save files.rustdata
```

A legacy command boundary destroys schema unless a parser reconstructs it:

```sh
cat access.log | parse nginx-log | schema
```

Before `parse`, the stream is text or lines. After `parse`, it is records.

## Script stability

Scripts SHOULD pin views and renderers rather than rely on user-configured
terminal presentation.

Prefer:

```sh
ls --view names >names.txt
ls | select name,size | render csv --header >files.csv
```

Avoid relying on terminal layout:

```sh
ls >names.txt       # acceptable for simple names
ls -l | awk ...     # legacy-compatible, but less robust than fields/select
```

A script may assert shape:

```sh
ls | require-schema tairix.fs.FileEntry@1 | select name,size
ls | require-fields name,size,modified
```

`require-schema` and `require-fields` are recommended utilities. They SHOULD
exit non-zero if the assertion is false.

## Builtins

The shell MUST include these builtins:

```sh
cd
pwd
exit
export
unset
echo
jobs
fg
bg
help
```

A builtin runs inside the shell process when it mutates shell state, such as
the environment, current directory, job table, or REPL exit request. Everything
else should be an external userland program unless performance or bootstrapping
requires a builtin implementation hidden behind identical behavior.

The shell MAY implement `schema`, `fields`, `headers`, `views`, `describe`,
`render`, and `save` as optimized builtins, but the corresponding user-visible
programs MUST exist.

The resource and storage management verbs — `show`, `describe`, `watch`,
`resolve`, `pin`, `unpin` and the namespace-specific wrappers `disk`, `tty`,
`stats`, `vol`, etc. (`plans/ALIAS.md` §15.4), and the storage/alias tools of
`plans/DRIVES.md` — are **external userland programs**, not shell builtins:
they resolve resources and mutate persistent alias state through
capability-checked services, which is not shell-process state. The shell only
parses their resource-reference and alias-path arguments (through the shared
parsers) and completes them (see Tab expansion and completion); it MUST NOT
reimplement resolution or alias storage in-process.

## Job control

A backgrounded pipeline (`&`) is added to the job table as running and prints:

```text
[N] pid
```

The foreground status updates `$?`. A foreground job reported as stopped by the
host becomes a stopped job. `fg` and `bg` resume jobs through `ProcessHost`.
Finished background jobs are reported before the next prompt:

```text
[N] Done command
```

Reports MUST NOT interleave in the middle of foreground command output.

## Failure handling

Lexical and parse failures are line-aborting errors. The line runs nothing and
sets `$?` to `2`.

Failures after a line is understood are ordinary command statuses. Examples:

- command not found: `127`
- command not executable: `126`
- permission denied during `cd`: non-zero builtin status
- unsupported launch feature in the current kernel ABI: `NotImplemented`
  mapped to a shell command failure

`stdinfo` write failures MUST NOT change `$?`.

Malformed `stdinfo` consumed by `schema`, `fields`, `headers`, or `views` MUST
be reported as untrusted malformed metadata. It MUST NOT be executed,
interpreted as policy, or used as instructions.

## Current implementation note

The `spawn` ABI carries the child's argument vector and environment (the
caller-encoded, kernel-revalidated startup-strings block), so the runtime
host passes a command's words and the shell's exported variables (with any
`NAME=v cmd` prefix overrides layered on top) to every launched program.
Job control is live end to end (`plans/SPAWN.md` SP7/SP9): the runtime host
delivers `Continue`/`Terminate`/`Kill` through the `signal` syscall, marks
its foreground child on fd 0 (`console_foreground`) around every blocking
wait so the kernel's cooked-mode line discipline routes `^C`/`^Z` to the
running job, and waits with `WaitFlags::STOPPED` so a `^Z`-stopped job
returns to the prompt as `WaitOutcome::Stopped` (`$?` = 148) and `fg`/`bg`
resume it. Pipes and redirections run end to end (`plans/SPAWN.md` SP10):
the pure `tairix_elsh::wireplan` planner lowers each pipeline into
pre-opened targets (`fs_open`/`resource_open`/`pipe_create`), one fd 0–3
wire map per member, and the here-string / multios byte pumps, and the
runtime host executes the plan over `spawn_attached` — all-or-nothing
opens, kill+reap unwind on a mid-pipeline refusal, transferred ends closed
after the last spawn, non-leader members reaped after the leader. The one
launch form still refused closed (`NotImplemented`) is a redirection or
duplication naming a `{var}` dynamic descriptor (fd ≥ 10): the spawn
attach block wires only the standard fd 0–3.

Redirection state:

- **Implemented and tested** — the fd-aware redirection model. The lexer
  decodes each operator into a `RedirOp` (carrying its explicit or default
  descriptor); the parser attaches the file target; and the interpreter lowers
  each `Redirection` to primitive `ResolvedRedirection { fd, action }` values
  (`Open`/`Dup`/`Close`) the host applies in order. Supported operators:
  `<`, `>`, `>>`, `<>`, the clobber-override spellings `>|`/`>!`/`>>|`/`>>!`,
  an optional glued IO number on any of them (`2>`, `3>>`, `0<`), descriptor
  duplication (`n>&m`, `n<&m`, `2>&1`), descriptor close (`>&-`, `<&-`,
  `n>&-`), and the combined stdout+stderr forms `&>`/`>&file`/`&>>`/`>>&` with
  their clobber spellings (lowered to an open on fd 1 plus a dup of fd 1 onto
  fd 2 — one definition of the combined meaning), and the here-string `<<<`
  (lowered to a `HereString` action carrying the expanded word plus one
  trailing newline — the one definition of the here-string's shape — on its
  descriptor, default fd 0). Fail-closed: a file redirection, here-string, or
  here-document with no target/delimiter word, or an ambiguous duplication
  (`<&file`, `2>&file`), runs nothing.
- **Implemented and tested** — multi-line here-documents (`<<`, `<<-`). The
  command line names only the delimiter (quote removal, never expansion; any
  quoted part — including a double-quoted run, tracked by the lexer's
  `Segment::QuotedExpandable` — makes the body literal, else the body gets
  the same `$` expansion as a word). The body is collected afterwards in
  source order (`CommandList::pending_here_doc` / `feed_here_doc_line`),
  driven by the REPL under a `> ` continuation prompt; `<<-` strips leading
  tabs from body and terminator lines. A complete body lowers to the same
  `HereString` bytes-on-fd action as a here-string — one primitive. Bounded
  and fail-closed: the body is capped by `MAX_HERE_DOC_BYTES` (64 KiB, a
  fixed security bound), an over-large or line-loss-poisoned body is
  discarded yet still consumed to its terminator (so body lines are never
  misread as commands) and fails the line with `HereDocTooLarge`, and an
  unterminated document (end of input first) fails with
  `UnterminatedHereDoc`; either way `$?` is 2 and nothing runs. Any
  run-stage line abort (failed expansion, missing body) now reports and
  sets `$?` through the same shared path as a parse error.
- **Implemented and tested** — redirection-target classification (the
  "Resolution rule" above). Each expanded `Open` target is classified into a
  `RedirTarget` (`Path` or `Resource`) through the single shared
  `lib/resref` parser, never a shell-private reference grammar (`AGENTS.md`
  §2.2). A relative target whose first path component holds a `:` preceded by a
  registered resource namespace and not immediately followed by `/`, with a
  prefix that is neither `.` nor `..`, is a resource reference (`sys:null`);
  every other spelling — absolute, `./x`, `foo/sys:x`, an unregistered prefix
  `foo:bar`, or the alias-path form `Home:/x` — stays a path, so no on-disk
  file whose name contains `:` is ever shadowed. A registered-namespace target
  that is not a well-formed reference (`sys:null@`) fails the whole line closed
  (`InvalidResourceTarget`) rather than falling back to creating a file.
- **Implemented and tested** — the required pipeline/command forms beyond the
  basic connectors: `a |& b` (lowered once, in the parser, to a `2>&1`
  duplication appended to the left command), the `!` status-negation prefix
  (`Pipeline::negated`, applied to the foreground `$?`; a doubled `!` negates
  again), and `NAME=VALUE` prefix assignments (split by the shared
  `split_prefix_assignments`; expanded against the pre-assignment environment,
  carried to an external launch as `ResolvedCommand::env_overrides` — the
  child's environment, never the shell's — and bound temporarily around a
  builtin). A `!` after the pipeline has begun is an `UnexpectedToken` parse
  error rather than a silently dropped word.
- **Implemented and tested** — zsh multios and `{var}` dynamic descriptor
  allocation. Repeated opens on one descriptor merge into a single
  `RedirAction::Multi` (targets keep their own modes and are classified
  independently, so `>log >sys:null` mixes a path and a resource); all-output
  targets fan out, all-input targets concatenate in order, and a descriptor
  mixing directions (or the bidirectional `<>`) fails the line closed. The
  host contract is fail-closed: open every target or apply nothing. A
  `{var}` glued to any `<`/`>` operator allocates a fresh descriptor (≥ 10,
  never fd 0–3) and binds its number to `$var`; `{var}>&-` closes the number
  read back from `$var` and fails closed (`BadDynamicFd`) when the variable
  does not hold an allocated descriptor.
- **Implemented and tested** — command resolution (`plans/APPS.md` §8–§9,
  the owning spec). The pure candidate policy
  (`tairix_cmdres::resolution_candidates`, the `lib/cmdres` definition the
  `man` command's bundle lookup also imports) computes the ordered program-path
  spellings for a command word — explicit paths (containing `/`) bypass the
  search, a trailing `.app` names the bundle and runs its `Run` binary, and
  a bare word resolves against the fixed, non-overridable five-step search order
  (§16.8): (1) /System/Commands, (2) /System/Applications, (3) <home>/Commands,
  (4) <home>/Applications, and (5) the user's PATH —
  and the runtime host attempts the candidates in order (`spawn`'s
  `NotFound` moves to the next, any other refusal is final). The interpreter
  maps a launch `NotFound` onto `127` "command not found" and every other
  launch refusal onto `126`, on the foreground and background paths alike.
  Because process launch is asynchronous (`plans/FIX-DESKTOP.md` DESK-1),
  `spawn` now only *admits* the child and its image is read/verified/built on
  the child's own task; an I/O or verification failure therefore surfaces as a
  reserved `LOAD_*` child-exit status, not a synchronous `spawn` `-errno`. The
  interpreter recognises that reserved status on reap and reports the failure
  loudly with the one shared human reason (`tairix_abi::load_failure_reason`)
  — `shell: <cmd>: <reason>` — mapping `$?` to `127` for a
  missing/unreadable program (`LOAD_NOT_FOUND`) and `126` for every other load
  refusal (verification, malformed image, out of memory). The foreground path
  reports on the terminal exit; a background job's refusal is stated on stderr
  as its `[N] Done` line is drained, so no launch failure is ever silent
  (`§24.1`).
- **Recognised and failing closed** (tracked here): process substitution —
  `<(…)`/`>(…)` await the pipe/launch plumbing and `=(…)` is permanently
  unsupported (no scratch filesystem, §16.1) — and the compound commands
  `( list )` / `{ list; }`. Each aborts the line with a parse error
  (`UnsupportedProcessSubstitution` / `UnsupportedCompound`), so a
  parenthesised command is never misread as a filename and `{`/`(` never runs
  as a program name. A redirection on a *builtin* also fails closed (status
  1): builtins write through the `Console` seam, and silently sending a
  redirected stream to the terminal would be worse than refusing. A classified
  `Resource` target resolves at launch through `resource_open` (the
  capability-checked kernel resolver), exactly as a `Path` target opens
  through `fs_open`; both are pre-opened in the shell's own table and wired
  into the child through the spawn attach block (`plans/SPAWN.md` SP10b).

## Tests

`cargo test -p tairix-elsh` MUST cover:

- zsh-style quoting and escapes
- comments
- variable expansion
- command connectors `;`, `&&`, `||`, newline, and `&`
- pipelines `|` and `|&`
- grouping with `( ... )` and `{ ...; }`
- all supported redirections, including fd 3
- zsh-only redirection operators: `>!`, `>>|`, `>>!`, `>&`, `>>&`, `&>|`,
  `&>!`, `<<-`, and `{var}>` dynamic fd allocation
- noclobber semantics: `>!`/`>>|`/`>>!`/`&>|`/`&>!` clobber, plain `>`/`>>`
  honor noclobber
- `{var}>` allocates an fd ≥ 10 and refuses to allocate fd 0/1/2/3
- multios: a repeated output redirection fans out to every target and fails
  closed if any target fails to open
- process substitution `<(...)` and `>(...)` resolve to stream backings, and
  `=(...)` fails closed as unsupported
- fd 3 capture, close, append, and best-effort no-consumer behavior
- redirection target namespace resolution: `sys:null`, `sys:zero`,
  `sys:full`, and `sys:random` resolve to stream backings
- resource-reference vs. path-alias vs. filesystem-path disambiguation:
  `/sys:x`, `./sys:x`, and `foo/sys:x` resolve to filesystem paths, bare
  `sys:x` resolves to a resource reference, and `Home:/x` resolves to a path
  alias (prefix followed by `/`), not a stream backing
- a non-stream resource in target position (e.g. an `info:`/`stats:` answer)
  fails closed rather than resolving
- an unknown leaf under a registered namespace (`sys:nul`) fails closed
  without creating a file
- an unregistered prefix (`foo:bar`) resolves to a filesystem path
- `cd` accepts alias paths (`Home:/Documents`), the synthetic view path
  (`/Users/ian`), and relative paths, and fails closed on malformed selectors
  (`Home:Documents`, `C:Users`); the shell keeps one current directory as a
  root+directory handle with no per-drive current directory
- prompt display renders an alias path when an alias maps to the current
  root and the stable ID otherwise
- resource-reference completion is namespace-aware, inserts an identity guard
  for a destructive command, and does not hide an identity mismatch
- parse-error fail-closed behavior
- builtin behavior and status propagation
- job control table behavior
- `stdinfo` JSONL emission from the REPL and representative commands
- `schema`, `fields`, `headers`, and `views` planner forms
- `schema`, `fields`, `headers`, and `views` `--from-stdinfo` forms
- `ls | schema`, `ls | fields`, `ls | headers`, and `ls | views` shorthand
- `ls >mylisting.txt` simple names rendering
- `ls -ls >mylisting.txt` blocks-long rendering
- renderer header and no-header behavior
- native-to-native negotiation
- native-to-legacy fallback
- tab completion for commands, paths, options, views, fields, predicates,
  renderers, and fd redirections
- completion safety: no side effects, no network, no secrets, no `$?` change
- malformed and hostile `stdinfo` being treated as untrusted data
- interactive rendering and key decoding go through the shared terminal stack
  (`lib/vt`/`lib/termcap`/`lib/curses`) with no second escape-sequence
  vocabulary, key table, or screen model
- capability-keyed rendering degrades attributes/colour to the active `TERM`'s
  capabilities (truecolour → 256 → 16 → mono)
- an unknown/missing `TERM` degrades to the safe baseline without panicking and
  without any `TERM`-derived file read
- a non-interactive `stdin`/`stdout` (pipe, file, `TERM=dumb`) degrades line
  editing and the completion menu to plain line input and plain text, with
  identical scripted/piped behavior
- the line editor and curses screen model never write to fd 3
- a typed resource value carries identity (kind, canonical identity, short
  fingerprint, facet rights, generation, scope), re-checks generation/identity
  at use time, and its text serialization carries identity but never authority

## Summary

The shell remains zsh-like where users expect zsh-like behavior:

```sh
ls >mylisting.txt
ls -ls >mylisting.txt
ls >sys:null
cmd 2>errors
cmd 3>info.jsonl
cmd | grep foo
cmd && next || fallback
```

It adds structured discovery without making text worse:

```sh
schema { ls; }
fields { ls; }
headers { ls -ls; }
views { ls; }
ls | select name,size | render csv --header >files.csv
```

It speaks the TAIRiX storage and resource namespaces instead of Unix device
files and a single root tree:

```sh
cd Home:/Projects/TAIRiX         # alias path into a named root (DRIVES.md)
cat Backup:/snapshots/latest/log
head -c 64 < sys:random          # a resource reference's stream facet (ALIAS.md)
disk info disk:backup@7K2M       # an identity-guarded resource reference
```

`stdout` carries primary data. `stderr` carries diagnostics. `stdinfo` on fd 3
carries optional advisory JSONL. Native tools may preserve typed records across
pipes, but files, terminals, and legacy tools get stable rendered text unless
the user explicitly asks for another representation.

Path spelling and named roots are owned by `plans/DRIVES.md`, typed resource
references by `plans/ALIAS.md`, and the terminal vocabulary by
`plans/CURSES.md`; this shell consumes all three through their single shared
parsers/libraries and never reimplements them.
