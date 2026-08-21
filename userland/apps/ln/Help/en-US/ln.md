## NAME

ln — create links between files

## SYNOPSIS

`ln [-srLPdFfinvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Creates a link naming each target. With one operand the link is made in
the working directory under the target's own name. With two, the second
operand is a directory to fill when it is one — or a link to one,
unless `-n` — and the link's name otherwise. With three or more, the
last must already be a directory.

Without `-s` the link is a **hard** one: a second directory entry for
the target's own inode. Both names reach one file, a write through
either is visible through the other, and the file's storage survives
until the last name is removed. Both names must be on one volume, and a
directory is never given a second name — the file tree staying a tree
is what makes `..` mean the directory you actually came through.

With `-s` the link is a **symbolic** one, and its target is stored
**verbatim** and never resolved: it may be relative, may contain `..`,
and may name nothing at all, so such a link may legitimately dangle.
Its grammar is still checked before it is stored, so a target no
resolver could ever walk is refused. Creating either kind grants no
authority over what it names — every later use is authorised component
by component under your own identity.

A link name that is already taken is refused unless `-f` or `-i` says
to replace it, and replacing it **removes** that name first, so nothing
travels through a link that was already there to whatever it points at.
A directory is never replaced.

The first failure stops the run before any later target; links already
made stay made. `--` ends option parsing: every later argument is an
operand.

`-r` stores a symbolic link's target relative to the link's own
directory. Both halves are canonicalised by the filesystem first, so
the difference between them is exact: two canonical paths hold no `..`
and no link. The same arithmetic on the operands as typed would name a
different object as soon as a link were involved. `-r` needs `-s`,
because a hard link stores no target to make relative.

`-b`/`-S` are refused because there is no backup machinery to invoke.

## OPTIONS

- `-s, --symbolic` — make symbolic links rather than hard ones.
- `-r, --relative` — store each symbolic link's target relative to the
  link's own directory. Requires `-s`.
- `-L, --logical` — hard-link what a symbolic-link target names,
  rather than the link itself.
- `-P, --physical` — hard-link the target exactly as spelled,
  following no final symbolic link. The default.
- `-d, -F, --directory` — accept a directory operand. The link is
  still refused: no user may give a directory a second name.
- `-f, --force` — remove an existing link name, then create the link.
- `-i, --interactive` — ask before removing an existing link name;
  only a reply beginning `y`/`Y` consents. The later of `-f` and `-i`
  wins.
- `-n, --no-dereference` — treat a destination that is a symbolic
  link to a directory as the plain name it also is, rather than a
  directory to create links in.
- `-v, --verbose` — report each link made as `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — create every link in `dir`,
  which must already be a directory. The value follows attached
  (`-tdir`, `--target-directory=dir`) or as the next argument.
- `-T, --no-target-directory` — treat the destination as a link name,
  never a directory to fill; exactly two operands. Cannot be combined
  with `-t`.
- `-h, -?, --help` — show this command's own short help.

## EXAMPLES

- `ln notes.txt notes.bak` — give one file a second name; removing
  either leaves the other, and its contents, intact.
- `ln -s /System/Commands/ls.app tools/ls` — symbolically link one
  name to a bundle.
- `ln -s ../shared/notes.txt` — link `notes.txt` here to a relative
  target.
- `ln -sv -t Links a.txt b.txt` — link both files into `Links`,
  reporting each link.
- `ln -sfn /Storage/media Music` — repoint an existing `Music` link at
  a new directory, replacing the link rather than linking inside it.

## EXIT STATUS

- `0` — every link was created (or the short help was written); a
  declined `-i` question is not a failure.
- `1` — anything else, with the reason printed on standard error. A
  command line that was not understood exits `1` too.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
