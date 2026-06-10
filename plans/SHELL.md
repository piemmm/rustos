# RustOS shell (`userland/shell/shell`)

`rustos-shell` is the default RustOS command interpreter. It should feel
familiar to users of zsh/POSIX shells while adding structured output
discovery, predictable rendering, and the RustOS standard information stream
(`stdinfo`, fd 3).

The shell has two equally important jobs:

1. Run ordinary shell commands with zsh-style syntax and Unix-like stream
   semantics.
2. Let users and tools discover the shape, fields, views, and renderings of a
   command's output without turning every pipeline into verbose object text.

This document is normative unless a section is explicitly marked as an
implementation note.

## Terminology

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as implementation requirements.

- **Primary data** means bytes written to `stdout` (fd 1).
- **Diagnostics** means errors and warnings written to `stderr` (fd 2).
- **Advisory information** means optional JSONL records written to `stdinfo`
  (fd 3).
- **Native stream** means a RustOS-aware structured stream carried on `stdout`.
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

The shell depends only on audited RustOS interface crates such as `lib/abi`.
A userland program MUST NOT link kernel or driver crates.

## Pure interpreter architecture

The shell decides what to run, how to expand words, how to connect streams,
and how to apply control-flow operators. It does not itself name devices,
open terminals, talk to UARTs, or call kernel facilities directly. Like every
RustOS text program, the shell performs all of its own text I/O over the four
inherited standard streams (fd 0/1/2/3) and never over a kernel-discovered
console, UART, or framebuffer device (`AGENTS.md` §20).

External effects are reached through injected seams:

- `ProcessHost`: launches, waits for, signals, and polls jobs; changes the
  shell working directory; exposes stable `Errno` failures.
- `Console`: the REPL's view of the shell's own inherited standard streams.
  It reads interactive command-line input from `stdin` (fd 0), writes the
  prompt and REPL `stdout` text to `stdout` (fd 1), and writes REPL
  diagnostics to `stderr` (fd 2). It is never a console/UART device handle
  (`AGENTS.md` §20).
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

1. Alias expansion.
2. Brace expansion.
3. Tilde expansion.
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
not a `/dev/fd`- or `/proc`-style path — RustOS has neither (`AGENTS.md`
§16.1). The named-pipe/anonymous-fd plumbing is owned by the shell and the
kernel IPC layer; no temporary filesystem entry is created. zsh's `=(...)`
temporary-file form is **not** supported, because it would require a writable
scratch path and RustOS has no `/tmp` (`AGENTS.md` §16.1); the shell MUST
report `=(...)` as an unsupported expansion and fail closed rather than invent
a scratch location.

fd 3 is named `stdinfo`. It remains an ordinary file descriptor for
redirection syntax, but it is reserved by convention and ABI for advisory
information about the command's `stdout` or operation.

## Redirection target namespaces

RustOS has no `/dev`. The only top-level directories are `/System`,
`/Users`, `/Apps`, and `/Storage`; `/dev`, `/proc`, `/sys`, `/etc`, and the
other legacy POSIX names are reserved and refused (`AGENTS.md` §16.1). The
Unix idiom of redirecting to or from a device file (`> /dev/null`,
`< /dev/zero`, `< /dev/random`) therefore cannot be a filesystem path in
RustOS. These sinks and sources are instead **stream backings**
(`AGENTS.md` §20): a redirection MAY name a *registered namespace* target
that the shell resolves to a kernel stream-backing object instead of a
path.

### Syntax

A redirection target of the form `name:leaf` whose `name` is a registered
namespace is resolved as a stream backing, not a filesystem path:

```sh
ls  > sys:null        # discard stdout
cmd < sys:zero        # read an endless run of 0x00
cmd < sys:random      # read CSPRNG bytes (AGENTS.md §22)
ls  > sys:full        # writes fail closed with NoSpace
```

`sys:` is the only registered namespace at this stage and it carries the
well-known streams `null`, `zero`, `random`, and `full`. The resolution
rule below is written for *whatever* prefix is registered; only a
registered prefix is ever special.

### Resolution rule (namespace vs. filesystem path)

The shell MUST apply this test before any VFS lookup. A target is resolved
as a namespace **only** when every clause holds; otherwise it is an
ordinary filesystem path:

```
if the target is a relative path
and its first path component contains ':'
and the substring before that ':' is a registered namespace
and that prefix is neither "." nor ".."
then resolve the target as a namespace stream backing
else resolve the target as a filesystem path
```

The consequence is that every real file stays addressable on every mounted
volume — nothing on disk is consulted for a namespace target, and a path
escape always exists:

| target            | resolves to                                              |
|-------------------|----------------------------------------------------------|
| `sys:random`      | namespace stream backing (relative, registered prefix)   |
| `/sys:random`     | absolute filesystem path (not a relative path)           |
| `./sys:random`    | filesystem path (first component is `.`)                 |
| `foo/sys:random`  | filesystem path (first component `foo` has no `:`)       |
| `foo:bar`         | filesystem path (`foo` is not a registered namespace)    |

This matters because `:` is a legal filename byte on ext2/3/4 and POSIX
volumes (only `NUL` and `/` are forbidden), so no printable sigil can be
"reserved" on disk. The rule reserves nothing on any filesystem; it only
gives a registered prefix a meaning in *target position*, and a real file
named `sys:random` is always reachable as `./sys:random` or when quoted.

### Well-known streams

The registered namespace targets resolve to a **closed** set of stream
backings defined once in `lib/abi` as a versioned, hashed,
frozen-on-release enum (`AGENTS.md` §9, §23.2) — never a string-keyed
device table:

| target        | read behavior         | write behavior        |
|---------------|-----------------------|-----------------------|
| `sys:null`    | immediate EOF         | bytes discarded       |
| `sys:zero`    | endless `0x00`        | bytes discarded       |
| `sys:full`    | endless `0x00`        | fails with `NoSpace`  |
| `sys:random`  | CSPRNG bytes (§22)    | bytes discarded       |

`sys:random` MUST draw from the single kernel random subsystem behind
`rustos_abi::random` (`AGENTS.md` §22). It MUST NOT introduce a second
entropy source, PRNG, or seeding path.

### Capabilities and failure

Acquiring a stream backing is a capability-checked syscall that returns a
descriptor wired to the chosen fd (`AGENTS.md` §5.4, §20); `null`, `zero`,
and `full` need no special capability, and `random` rides the §22 API's
own checks. There is no ambient "open any device" surface (`AGENTS.md`
§4).

Resolution MUST fail closed (`AGENTS.md` §5.4):

- A target with a **registered** namespace prefix but an **unknown leaf**
  (e.g. `sys:nul`) MUST be a hard error. The shell MUST NOT fall back to
  creating a file, so a typo can never silently produce junk on disk.
- An **unregistered** prefix is not special: it is an ordinary filesystem
  path, exactly as the resolution rule states.

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

Security events MUST go through the RustOS logging facility, not fd 3. AI and
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
  through the RustOS logging facility, never fd 3 (`AGENTS.md` §20.1)
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
  "schema_id": "rustos.fs.FileEntry@1",
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
`rustos_abi::stdinfo`. The `ai` object is producer-defined but SHOULD follow
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
| `native`         | compact RustOS structured stream                 |

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

### Required completion contexts

At command position:

```sh
<TAB>
```

Suggest builtins, functions, aliases, external commands, and executable paths.

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

Suggest paths. For `3>`, the completion menu SHOULD annotate the target as a
`stdinfo` JSONL capture and MAY prefer `.jsonl` names, but it MUST still allow
any valid path.

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
ls | require-schema rustos.fs.FileEntry@1 | select name,size
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

An early RustOS spawn ABI may carry only a program path, without argv,
environment, pipes, redirections, or job-control signals. In that environment,
the runtime host may fail closed with `NotImplemented` for external commands
that need richer launch support. The shell parser and in-process builtins
should still implement and test the target semantics described here.

## Tests

`cargo test -p rustos-shell` MUST cover:

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
- namespace-vs-path disambiguation: `/sys:x`, `./sys:x`, and `foo/sys:x`
  resolve to filesystem paths, while bare `sys:x` resolves to a namespace
- an unknown leaf under a registered namespace (`sys:nul`) fails closed
  without creating a file
- an unregistered prefix (`foo:bar`) resolves to a filesystem path
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

`stdout` carries primary data. `stderr` carries diagnostics. `stdinfo` on fd 3
carries optional advisory JSONL. Native tools may preserve typed records across
pipes, but files, terminals, and legacy tools get stable rendered text unless
the user explicitly asks for another representation.
