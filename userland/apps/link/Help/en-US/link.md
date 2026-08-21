## NAME

link — give a file a second name

## SYNOPSIS

`link [--] existing new`

## DESCRIPTION

Creates one hard link: `new` becomes a second name for the node `existing`
already names. Both names then reach the same file — a write through one
is visible through the other, because there is one file, not a copy — and
the file's storage survives until the last of its names is removed.

There are deliberately no options. `ln` is the tool with `-f`, `-i`, `-v`,
`-s`, `-L`/`-P` and the `-t`/`-T` destination forms; keeping them separate
means a script that must create one hard link and nothing else gets a tool
that cannot replace a name, follow a link, or make a symbolic link
instead.

Neither name is followed. `existing` is the node **as spelled**, so a
symbolic link planted there cannot redirect the new name at what it points
to (`ln -L` is the tool for the following posture). `new` is a name being
created, so an occupied one is refused, never replaced.

The refusals each say something different:

- the new name already exists — a create never replaces a name;
- `existing` is a **directory** — a directory has exactly one name
  everywhere, so no principal may give one a second;
- the two names are on **different volumes** — a node's second name must
  live on the volume that stores it;
- the format's per-node name count would overflow;
- the filesystem stores **one name per node** — a permanent property of
  that format, not a transient failure. Use `ln -s` for a symbolic link
  there.

Exactly two operands are required; anything else is a usage error and no
link is made. `--` ends option parsing, so a name that begins with a dash
may be linked.

## OPTIONS

- `-?, --help` — show this command's own short help.

## EXAMPLES

- `link report.txt report-backup.txt` — a second name for one file.
- `link -- -odd-name second` — link a name that begins with a dash.

## EXIT STATUS

- `0` — the link was created (or the short help was written).
- `1` — the filesystem refused the link, or the output failed; the reason
  is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
