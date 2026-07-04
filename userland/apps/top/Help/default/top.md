## NAME

top — watch the process list live

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Shows a live, full-screen view of the process list through the System
Information API, in the spirit of the classic `top`. It starts on the
caller's own processes; the system-wide view is granted by the service
only to a caller holding `CAP_SYSINFO_GLOBAL`.

The viewer takes no operands: it is controlled with keys pressed inside
the session.

- `q` — quit.
- `a` — toggle between your own processes and the system-wide view.
- `r` — refresh the listing.
- Up/Down, PageUp/PageDown, Home/End — move the selection.
- `h`, `?` — toggle the in-session key overlay.

## OPTIONS

- `-h, -?` — show this command's own short help and exit. Inside a
  running session the same keys toggle the key overlay instead.

## EXIT STATUS

- `0` — the session ended with `q`, or the short help was shown.
- `1` — the service or the terminal failed.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
