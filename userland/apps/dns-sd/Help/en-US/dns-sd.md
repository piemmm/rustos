## NAME

dns-sd — browse, resolve, and look up link-local services

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Asks the local network — the link — about the services it offers, through
the system's link-local discovery service. `dns-sd` never speaks multicast
DNS itself: the discovery service asks the segment on its behalf and admits
each request against the program's own authority.

With `-B` it browses for the instances of one service type, such as
`_ipp._tcp` for printers, and prints each instance as it is added or removed.
With no type it lists every service type the link offers. With `-L` it
resolves one instance to the host and port it is reached at and prints what
the instance says of itself (its `TXT` attributes). With `-G` it looks up the
addresses of a host under `local`.

Each answer names the interface it was learned on, and a `Flush` line means
everything learned on that interface is no longer known — its link went down,
or the service started over. Every name printed was chosen by another machine
on the link, so it is shown escaped, in DNS presentation form: a space as
`\032`, a control character as its decimal code.

Browsing every type, or a type the running account has not been granted, needs
`CAP_NET_DISCOVER_ALL`, which only an administrator's account carries. The only
domain on the link is `local`.

## OPTIONS

- `-B` — browse for the instances of a service type, or every type when none
  is given.
- `-L` — resolve one instance of a service type.
- `-G` — look up a host's IPv4 (`v4`), IPv6 (`v6`), or both (`v4v6`)
  addresses.
- `-t` — stop after this many seconds instead of running until interrupted.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — the printers on the link, as they come and go.
- `dns-sd -t 5 -B` — every service type seen within five seconds.
- `dns-sd -L "Hall Printer" _ipp._tcp` — where one printer is reached.
- `dns-sd -G v4v6 printer.local` — a host's addresses.

## EXIT STATUS

- `0` — the command ran its course (or the short help was written).
- `1` — the discovery service refused the request or is not running.
- `2` — the command line was not understood, or the output could not be
  written.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
