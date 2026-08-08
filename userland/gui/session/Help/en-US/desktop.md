## NAME

desktop — start the graphical desktop session

## SYNOPSIS

`desktop`

## DESCRIPTION

Starts the graphical desktop session on this machine's seat: the
command acquires the seat's exclusive display and input lease, connects
to the display service, and runs the compositing desktop — the window
manager and the taskbar — until the session ends. The command returns
when the desktop session ends.

The same desktop starts automatically after authentication: a graphical
login (`os.loginType`) is the default on a machine that can run one.
This command starts it on demand from a text shell.

When no display service is running, or another session already holds
the seat, the command fails with its reason on standard error — it
never displaces a running session.

## OPTIONS

- `-h, -?` — show this command's own short help.

## EXAMPLES

- `desktop` — start the desktop session.

## EXIT STATUS

- `0` — the short help was served.
- `2` — the command line was not understood.
- any other non-zero code — the session could not start (no seat, no
  display service) or ended (the seat lease was lost); the reason is
  written to standard error.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `configure`
- `man`
