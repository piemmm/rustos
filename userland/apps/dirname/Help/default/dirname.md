## NAME

dirname — strip the last component from names

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Prints each path spelling with its last component removed: trailing
slashes are stripped, then the last component and the slashes before
it. The surgery is purely lexical — no path is resolved or touched on
disk. A spelling with no remaining slash has the parent `.`; a parent
that empties out is the root.

A root is never stripped into: `dirname /tools` is `/`, and — the
RustOS storage forest's equivalent — `dirname Home:/tools` is `Home:/`.
An alias root (`Home:/`, `System:/`, …) plays exactly the role `/`
plays on POSIX systems.

## OPTIONS

- `-z, --zero` — end each result with NUL instead of a newline.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `dirname /System/Apps/top.app` — print `/System/Apps`.
- `dirname src/lib.rs` — print `src`.
- `dirname file` — print `.` (no directory part).
- `dirname Home:/tools` — print `Home:/` (a root is never stripped
  into).

## EXIT STATUS

- `0` — the results (or short help) were written.
- `1` — the output could not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `basename`
- `man`
