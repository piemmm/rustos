# `tairix-ln` — create links between files

A `plans/SYMLINKS.md` S4/S6 command app, shipped as the self-contained store
bundle `/System/Commands/ln.app/` so the shell resolves the bare word `ln` to
it. `ln` is the GNU coreutils tool: it creates a link naming a target, in
every operand shape the GNU tool accepts — `ln target`,
`ln target link_name`, `ln target... directory`, and `-t`/`-T` — with `-s`,
`-L`, `-P`, `-d`/`-F`, `-f`, `-i`, `-n`, `-v`, and `--`. `-h`/`-?`/`--help`
render the tool's own short help from its bundled `Help/` tree through the
shared `lib/help` engine, in the locale the inherited `LANG` variable names.

## Both kinds of link

Without `-s` the link is a **hard** one (`fs_link`): a second directory entry
for the target's own inode, so both names reach one file and its storage
survives until the last name goes. `-L` gives the second name to what a
symbolic target *names*; `-P`, the default, links the target as spelled and
follows no final link — POSIX `link()` against `linkat(AT_SYMLINK_FOLLOW)`.
With `-s` the link is a **symbolic** one (`fs_symlink`), whose target is
stored verbatim and never resolved.

`-d`/`-F` accept a directory operand, matching the GNU tool, but the link is
still refused with `IsADirectory`: no principal may give a directory a second
name, because the tree staying a tree is what makes the resolver's physical
`..` well-defined. Both names must also lie on one volume — a directory entry
addresses an inode in its own backing — so a pair that crosses one is
`CrossVolume`.

`-r`/`--relative` stores a symbolic link's target relative to the link's own
directory. It asks the **kernel** to canonicalise both halves first
(`fs_realpath`) and then spells the difference: two canonical paths hold no
`..` and no link, so the arithmetic is exact. Doing it on the operands as
typed would be the lexical-`..` collapse the resolver forbids
(`plans/SYMLINKS.md` decision 4) — it would name a different node the moment
a link were involved. `-r` without `-s` is a usage error, because a hard link
stores no target to make relative.

One GNU switch group stays refused rather than approximated, for a reason
that is not about links: `-b`/`--backup` and `-S`/`--suffix`, because this
workspace has no backup machinery at all (`cp` and `mv` omit them for the
same reason), so a "backup" would be a name the tool invented. The refusal
is a usage error naming the switch; nothing is silently ignored, and the
divergence is documented in the tool's `Help/` documents.

## A target is data, not a path the tool walks

The target is stored **verbatim**: it may be relative, may carry `..`, and
may name nothing at all, so `ln -s` can legitimately create a dangling link
and never pre-validates what it names. Its *grammar* is checked kernel-side
before it is stored, so a target no resolver could walk is refused rather
than written. Creating a link grants no authority over what it names —
authority is decided at each later use, per component, under the caller's
attested identity.

## Replacing a name removes it

`-f` (and an approved `-i`) **remove** the existing name before creating the
link, so nothing ever travels through a link that was already there to
whatever it points at. A directory is never replaced: a directory
destination receives links inside it, and one that still blocks the name
(under `-T`) is reported, not removed. An unanswerable `-i` question is
never consent.

The first failure stops the run before any later target (fail closed), as in
`cp` and `mv`; links already created stay created.

## Layering & safety

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-abi`, `tairix-path`, and `tairix-help` crates, so it never
links a kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ` (the one line `-i` reads), and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode under the caller's attested identity.

## Usage

```
ln -s [-finvT] [-t dir] [--] target... [link_name]

  -s, --symbolic             make symbolic links (required, see above)
  -f, --force                remove an existing link name and retry
  -i, --interactive          ask before removing an existing link name
  -n, --no-dereference       treat a link-to-directory destination as a name
  -v, --verbose              report each link made
  -t dir, --target-directory=dir
                             create every link in dir
  -T, --no-target-directory  treat the destination as a link name
  -h, -?                     show this command's own short help
```

## Exit status

- `0` — every link was created (or the short help was written).
- `1` — anything else, with the reason on standard error. GNU `ln` has no
  separate usage status, so a malformed command line exits `1` too.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; the divergences
above are deliberate and documented in its `Help/` documents.
