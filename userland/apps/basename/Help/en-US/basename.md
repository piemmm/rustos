## NAME

basename — strip directory and suffix from names

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Prints the final component of each path spelling: trailing slashes are
removed, then everything up to and including the last remaining slash.
The surgery is purely lexical — no path is resolved or touched on disk.
With a `suffix` (the second operand, or `-s`), a trailing `suffix` is
also removed, unless it is the whole remaining name.

A root is never stripped into: `basename /` is `/`, and — the TAIRiX
storage forest's equivalent — `basename Home:/` is `Home:/`. An alias
root (`Home:/`, `System:/`, …) plays exactly the role `/` plays on
POSIX systems.

Without `-a` or `-s`, at most two operands are accepted: the name and
an optional suffix. With `-a` (or `-s`, which implies it), every
operand is a name.

## OPTIONS

- `-a, --multiple` — treat every operand as a name.
- `-s, --suffix <suffix>` — remove a trailing `suffix` from each name;
  implies `-a`. Also spelled `--suffix=<suffix>` or bundled (`-s.rs`).
- `-z, --zero` — end each result with NUL instead of a newline.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `basename /System/Apps/top.app` — print `top.app`.
- `basename src/lib.rs .rs` — print `lib`.
- `basename -s .rs -a a.rs b.rs` — print `a` and `b`.
- `basename Home:/` — print `Home:/` (a root is never stripped into).

## EXIT STATUS

- `0` — the results (or short help) were written.
- `1` — the output could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `dirname`
- `man`
