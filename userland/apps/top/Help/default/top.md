## NAME

top — watch the process list live

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Shows a live, full-screen view of the process list through the System
Information API, in the spirit of GNU `top`. It starts on the
caller's own processes; the system-wide view is granted by the service
only to a caller holding `CAP_SYSINFO_GLOBAL`.

The viewer takes no operands: it is controlled with keys pressed inside
the session.

- `q` — quit.
- `a` — toggle between your own processes and the system-wide view.
  If the service refuses the system-wide view (it requires
  `CAP_SYSINFO_GLOBAL`), the viewer stays on your own processes and the
  status line says why; the session keeps running.
- `r` — refresh the listing.
- Up/Down, PageUp/PageDown, Home/End — move the selection.
- `h`, `?` — toggle the in-session key overlay.

Three summary lines precede the list: the uptime, logged-in-user count,
and 1/5/15-minute load averages; the task census by state; and the
memory figures in MiB. The memory line needs `CAP_SYSINFO_KERNEL` — a
caller without it sees the refusal spelled out and the session
continues.

The rows are sorted by `%CPU`, biggest consumer first, and carry:

- `PID` — the numeric process id.
- `USER` — the owning account's username, resolved from the system's
  account directory; the numeric uid stands in when the name cannot be
  resolved.
- `SIZE` — the memory mapped in the process's address space (image,
  stack, and heap alike).
- `S` — the state letter: `R` running (green), `r` runnable, waiting
  for a CPU (cyan), `S` sleeping, `T` stopped (yellow), `Z` zombie
  (magenta). Colours appear on a colour terminal only; the letter
  itself always carries the state.
- `%CPU` — the CPU share over the interval since the previous refresh.
- `WCPU` — the weighted (exponentially smoothed) CPU share across
  refreshes, steadier than the instantaneous column.
- `TIME+` — cumulative CPU time, as `minutes:seconds.hundredths`.
- `COMMAND` — the process name.

## OPTIONS

- `-h, -?` — show this command's own short help and exit. Inside a
  running session the same keys toggle the key overlay instead.

## EXIT STATUS

- `0` — the session ended with `q`, or the short help was shown.
- `1` — the service or the terminal failed; the reason is printed on
  standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
