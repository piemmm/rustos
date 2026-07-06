## NAME

ps — list processes

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Lists processes through the System Information API. By default only the
caller's own processes are listed; the service applies every
per-query scope against the caller's kernel-attested identity, and
there is no path that bypasses that check.

Each process is printed as one row under a column header: the process
id (`PID`), the parent process id (`PPID`), the owning user and group
ids (`UID`, `GID`), the scheduling state (`S`), the CPU the process
last ran on (`CPU`), and the command name (`NAME`).

`ps` takes no operands.

## OPTIONS

- `-e, -A, --all` — list every process on the system rather than only
  the caller's own; the service grants this view only to a caller
  holding `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `ps` — list your own processes.
- `ps -e` — list every process on the system.

## EXIT STATUS

- `0` — the listing was written.
- `1` — the service refused or failed, or the listing could not be
  delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
