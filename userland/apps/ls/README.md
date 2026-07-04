# `rustos-ls` — list directory contents

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`),
registered at `/System/Apps/ls.app/Run` so the shell resolves the bare
word `ls` to it. `ls` inspects each of its path operands in order. A
non-directory operand is listed by name; a directory operand has its
entries listed, sorted by name. With no operand it lists the current
directory (`.`). With `-a` it includes entries whose name begins with
`.`; with `-l` it prints the long format — the type and permission bits,
the size, then the name — the POSIX model. `-h`/`-?` render the tool's
own short help from its bundled `Help/` tree through the shared
`lib/help` engine (`plans/APPS.md` §4).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` vocabulary and the shared
`rustos-help`/`rustos-vt` engines, so it never links a kernel or driver
crate (`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE`
plus `CAP_FS_ACCESS` — within the session baseline — and the secured VFS
authorises every path per-inode under the caller's attested identity.

## Usage

```
ls [-a] [-l] [--] [path...]

  -a, --all    do not hide entries whose name begins with `.`
  -l, --long   long format: type and permission bits, size, then name
  -h, -?       show this command's short help (also `--help`)
```

With no path operand `ls` lists the current directory. Short options may
be combined (e.g. `-la`). `--` ends option parsing: every later argument
is a path.

## A render machine, not a data source

`run` asks the injected filesystem seam for the metadata of each operand
and the entries of each directory, then writes the sorted, formatted
listing to the terminal. The operations that reach the outside world are
injected seams, mirroring the other userland crates (`cat`'s
`FileSource`, `man`'s `BundleStore`, `sysinfo`'s `Transport`):

- `Listing` — stat a path and read a directory's whole listing in one
  call, mirroring the kernel's one-shot `fs_readdir` contract. An
  entry's kind is the VFS's own `FileKind` (no parallel kind enum); the
  long format's per-entry mode and size come from a per-entry stat, paid
  only when `-l` asks for them.
- `Output` — write the rendered listing to the terminal and advisory
  records to the standard information stream (fd 3), best-effort.
- `rustos_help::HelpSource` — the tool's own `Help/` tree, read by the
  short-help switches.

On a running system these are syscall-backed (`src/run.rs`: `fs_open`/
`fs_stat`/`fs_readdir` and the inherited standard streams); in tests
they are in-memory fixtures, so every parsing, filtering, sorting, and
formatting decision is testable without a kernel.

## Layout

When several operands are given, non-directory operands are listed first
(sorted by name), then each directory operand has its entries listed,
preceded by a `path:` header and separated from the previous block by a
blank line. A single directory operand is listed without a header.

## Advisory output (`stdinfo`, fd 3)

When the default dotfile filter hides entries, `ls` emits the canonical
`fs.hidden_entries_omitted` omission record (`AGENTS.md` §20.1): a terse
human note with the `ls -a` suggestion plus structured data for tools.
Advisory only — never affecting the listing, ordering, or exit status.

## Help tree

`Help/<locale>/ls.md` carries the canonical `default/` (en-US) document
plus the required translations (`fr-FR`, `de-DE`, `es-ES`, `uk-UA`,
`it-IT`, `plans/APPS.md` §8.1). `src/help.rs` embeds the tree; the image
builder (`tools/mkimage`) and the QEMU image fixture plant those same
bytes at `/System/Apps/ls.app/Help/`, so image and source cannot drift.

## Fail closed

An unknown option is a `LsError::Usage` that inspects nothing. An
operand (or, under `-l`, a directory entry) that cannot be stat'd
surfaces the underlying `Errno` as `LsError::Stat` and stops before any
later operand. A directory that cannot be read is `LsError::Read`; a
directory stream carrying a non-UTF-8 name (an ABI-contract violation)
is refused whole rather than silently thinned. A failed terminal write
is `LsError::Output`. A missing own-help tree degrades `-h` to the usage
banner. There is no partial-guess path and no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-ls` drives the parser, the listing engine, and the
embedded help tree against in-memory fixtures; the aarch64
session-ceiling QEMU vertical types `ls /System/Apps` in a real session
and sees `man.app` in the listing.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
