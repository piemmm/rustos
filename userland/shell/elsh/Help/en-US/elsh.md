## NAME

elsh — the RustOS command shell

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Runs an interactive command shell — a read-eval-print loop over the
inherited standard streams. A typed command word is resolved first
against the shell's builtins, then the system app store
(`/System/Apps`), then the directories of the `PATH` variable; the
store is searched before `PATH`, so `PATH` can never shadow a system
command. An unresolved word exits `127`; a resolved but non-executable
bundle exits `126`.

The builtins:

- `cd <path>`, `pwd` — change and print the working directory.
- `echo ...` — print its operands.
- `export NAME=value`, `unset NAME` — edit the exported environment.
- `jobs`, `fg`, `bg` — job control.
- `ulimit` — read and impose resource limits.
- `elevate` — run one command re-authenticated through the console's
  login supervisor.
- `help` — list the builtins.
- `exit [code]` — end the session.

The shell takes no operands: script execution is not yet part of its
grammar.

## OPTIONS

- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

- The `exit` builtin's code, or `0` when the input stream ends (or the
  short help was shown).
- `2` — the invocation was not understood.

## ENVIRONMENT

- `PATH` — the directories searched after the system app store.
- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`), exported to every launched command.

## SEE ALSO

- `man`
