# `tairix-ls` — list directory contents

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`),
registered at `/System/Commands/ls.app/Run` so the shell resolves the bare
word `ls` to it. `ls` inspects each of its path operands in order. A
non-directory operand is listed by name; a directory operand has its
entries listed, sorted by name (or by size under `-S`), unless `-d`
names the directory itself. With no operand it lists the current
directory (`.`). The option surface follows GNU coreutils (`AGENTS.md`
§16.7): `-a`/`-A` reveal dotfiles, `-l` (and `-n`/`-g`/`-o`) select the
long format, `-h` scales sizes, `-R` recurses, `-r` reverses, `-F`/`-p`
append indicators, `-Q` quotes, and `-C`/`-x`/`-m`/`-1` (with `-w` for
the width) pick the arrangement. The default is GNU's: multiple columns
when output is a terminal, one name per line otherwise.
`-?`/`--help` render the tool's own short help from its bundled `Help/`
tree through the shared `lib/help` engine (`plans/APPS.md` §4).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help` engine, so it never links a kernel or driver
crate (`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE`
plus `CAP_FS_ACCESS` — within the session baseline — and the secured VFS
authorises every path per-inode under the caller's attested identity.

## Usage

```
ls [-aACdFghlmnopQrRsSx1] [-w cols] [--] [path...]

  -a, --all              do not hide entries whose name begins with `.`
  -A, --almost-all       like -a, but never list `.` or `..`
  -C                     columns, filled top-to-bottom (terminal default)
  -d, --directory        list directory operands themselves
  -F, --classify         append `/` to directories, `*` to executables
  -g                     long format without the owner column
  -h, --human-readable   with -l, sizes like `1.1K`, `23M`
  -l                     long format: mode, owner, group, size, name
  -m                     comma-separated names, wrapped to the width
  -n, --numeric-uid-gid  long format, numeric owner/group (same as -l)
  -o                     long format without the group column
  -p                     append `/` to directories
  -Q, --quote-name       double-quote each name
  -r, --reverse          reverse the sort order
  -R, --recursive        list subdirectories recursively
  -s, --size             allocated blocks per entry, with a `total` line
  -S                     sort by size, largest first
  -w, --width <cols>     output width in columns (0 = unlimited)
  -x                     columns, filled left-to-right
  -1                     one name per line (default when not a terminal)
  -L, --dereference      show what each symbolic link names, not the link
  -H, --dereference-command-line
                         dereference only the command-line operands
  -?                     show this command's short help (also `--help`)
```

When output is a terminal, entries are laid out in columns sized to the
attested terminal width (`-C`, the default); when output is a pipe or a
file, they are listed one per line. `-x` fills across, `-m` lists them
comma-separated, and `-w`/`--width` overrides the width (attested width,
else 80). The width is only ever read from the kernel's fail-closed
geometry attestation — an unattested console degrades to one-per-line
rather than guessing.

With no path operand `ls` lists the current directory. Short options may
be combined (e.g. `-la`). `--` ends option parsing: every later argument
is a path. The long format has no link-count column (this filesystem
contract carries no hard links, so a count would be fabricated) and
renders owner/group as numeric ids — the GNU numeric fallback — because
name resolution needs the capability-gated user database.

## Symbolic links

A link renders with the type letter `l` and, in the long format, as
`name -> target`: the target exactly as it was stored, unresolved, which
is what the link holds. The four GNU dereference postures are all
implemented, and which one is in force decides what every row is:

- `-l`, `-d`, and `-F` show **every** link as itself — the only reading
  under which a *dangling* link can be described at all.
- Otherwise a command-line operand that is a link **to a directory** is
  resolved, so `ls linkdir` lists the directory while a link to a file, or
  one that dangles, still shows itself.
- `-H` resolves every command-line operand; links inside a listing still
  show themselves.
- `-L` resolves every link wherever it appears — which is also the only
  posture under which `-R` can walk into a directory a link names, so a
  directory reached a second time through its own chain is reported
  (`not listing already-listed directory`) and not descended.

A path that cannot be inspected or read never ends the listing: the reason
goes to standard error, that path is skipped, and the remaining operands
and entries are still listed. A skipped *entry* renders with its type
letter and `?` for every cell a stat would have filled — never a
fabricated zero. The exit status is the GNU grade: `0` when everything
listed, `1` for a problem inside a listing, `2` for a command-line operand
that could not be reached (or a usage error).

## A render machine, not a data source

`run` asks the injected filesystem seam for the metadata of each operand
and the entries of each directory, then writes the sorted, formatted
listing to the terminal. The operations that reach the outside world are
injected seams, mirroring the other userland crates (`cat`'s
`FileSource`, `man`'s `BundleStore`, `sysinfo`'s `Transport`):

- `Listing` — stat a path (in the `stat` or `lstat` reading the posture
  selects, per path), read a link's stored target, and read a directory's
  whole listing in one call, mirroring the kernel's one-shot `fs_readdir`
  contract. An entry's kind is the VFS's own `FileKind` (no parallel kind
  enum); the per-entry stat behind the long format's columns, the `-S`
  size sort, `-F`'s execute-bit check, and `-L`'s resolution is paid only
  when one of them asks for it, and a link's target is read only by the
  format that prints it.
- `Output` — write the rendered listing to the terminal, a skipped path's
  reason to the error stream, and advisory records to the standard
  information stream (fd 3), best-effort.
- `tairix_help::HelpSource` — the tool's own `Help/` tree, read by the
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

`Help/<locale>/ls.md` carries the canonical `en-US/` document
plus the required translations (the `tairix_help::REQUIRED_LOCALES`
set, `plans/APPS.md` §8.1). The tree is authored on disk only:
`tools/syshelp` discovers it and the image builder (`tools/mkimage`) and
the QEMU image fixture plant it at `/System/Commands/ls.app/Help/`; the
binary embeds no help bytes (`plans/APPS.md` §6.1).

## Fail closed

An unknown option is a `LsError::Usage` that inspects nothing. An
operand (or a directory entry, when a per-entry stat is needed) that
cannot be stat'd surfaces the underlying `Errno` as `LsError::Stat` and
stops before any later operand. A directory that cannot be read is
`LsError::Read`; a directory stream carrying a non-UTF-8 name (an
ABI-contract violation) is refused whole rather than silently thinned. A
failed terminal write is `LsError::Output`. A missing own-help tree
degrades `-?` to the usage banner. There is no partial-guess path and no
panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p tairix-ls` drives the parser, the listing engine, and the
on-disk help tree against in-memory fixtures; the aarch64
session-ceiling QEMU vertical types `ls /System/Commands` in a real session
and sees `man.app` in the listing.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
