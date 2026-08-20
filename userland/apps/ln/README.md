# `tairix-ln` — create symbolic links

A `plans/SYMLINKS.md` S4 command app, shipped as the self-contained store
bundle `/System/Commands/ln.app/` so the shell resolves the bare word `ln` to
it. `ln` is the GNU coreutils tool: it creates a symbolic link naming a
target, in every operand shape the GNU tool accepts —
`ln -s target`, `ln -s target link_name`, `ln -s target... directory`, and
`-t`/`-T` — with `-f`, `-i`, `-n`, `-v`, and `--`. `-h`/`-?`/`--help` render
the tool's own short help from its bundled `Help/` tree through the shared
`lib/help` engine, in the locale the inherited `LANG` variable names.

## `-s` is required: this system has no hard links

There is no `fs_link` syscall and no driver call behind one, so `ln` without
`-s` has **nothing to create**. It says so and creates nothing, rather than
quietly making a symbolic link: a link and a second name for one inode are
different objects, and substituting one for the other would be a different
operation wearing the user's spelling. The same reasoning refuses the
hard-link-only switches `-L`, `-P`, `-d`, and `-F` — they select between
readings of a hard link's target, of which there are none here.

Two further GNU switches are deliberately refused rather than approximated:

- `-b`/`--backup` and `-S`/`--suffix` — this workspace has no backup
  machinery at all (`cp` and `mv` omit them for the same reason), so a
  "backup" would be a name the tool invented.
- `-r`/`--relative` — computing a target relative to the link's own
  directory needs a canonicalising path resolution (`realpath`) the ABI does
  not offer. A *lexical* approximation would name a different node the moment
  a link were involved, which is exactly the lexical-`..` collapse the
  resolver forbids (`plans/SYMLINKS.md` decision 4).

Every refusal is a usage error naming the switch; nothing is silently
ignored. The divergences are documented in the tool's `Help/` documents.

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
