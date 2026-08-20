## NAME

ln — create symbolic links

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Creates a symbolic link naming each target. With one operand the link
is made in the working directory under the target's own name. With two,
the second operand is a directory to fill when it is one — or a link to
one, unless `-n` — and the link's name otherwise. With three or more,
the last must already be a directory.

The target is stored **verbatim** and is never resolved: it may be
relative, may contain `..`, and may name nothing at all, so a link may
legitimately dangle. Its grammar is still checked before it is stored,
so a target no resolver could ever walk is refused. Creating a link
grants no authority over what it names — every later use is authorised
component by component under your own identity.

A link name that is already taken is refused unless `-f` or `-i` says
to replace it, and replacing it **removes** that name first, so nothing
travels through a link that was already there to whatever it points at.
A directory is never replaced.

The first failure stops the run before any later target; links already
made stay made. `--` ends option parsing: every later argument is an
operand.

`-s` is required on this system, which has no hard links: without it
there is nothing for `ln` to create, and it says so rather than making
a symbolic link, which is a different object. The hard-link-only
switches `-L`, `-P`, `-d`, and `-F` are refused for the same reason.
`-b`/`-S` are refused because there is no backup machinery to invoke,
and `-r` because computing a target relative to the link's own
directory needs a canonicalising resolution this system does not offer
— a lexical one would name a different object as soon as a link were
involved.

## OPTIONS

- `-s, --symbolic` — make symbolic links. Required: see above.
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

- `ln -s /System/Commands/ls.app tools/ls` — link one name to a
  bundle.
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
