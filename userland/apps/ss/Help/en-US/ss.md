## NAME

ss — list open sockets

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Lists the system's open sockets, one row per socket: the transport
protocol, the connection state, the receive and send queue depths, the
local and peer `address:port`, and — with `-p` — the owning process.

The rows come from the System Information API's socket listing, which
the network stack answers as a privileged, audited query: it names every
principal's sockets and every connection's peer, so listing all sockets
requires `CAP_SYSINFO_GLOBAL`. There is no `/proc/net`; a session without
that capability is told so and `ss` exits, rather than printing an empty
table.

By default the listing shows connected, non-listening sockets. `-l`
shows only listening sockets and `-a` shows both; the hidden-listener
count is noted on the standard information stream (fd 3), never in the
table. `-t` and `-u` restrict the protocol and `-4`/`-6` the address
family; with none given, every protocol and family is shown. Ports and
addresses are always numeric (TAIRiX has no service-name database), so
`-n` is accepted but always in force. An unspecified address prints as
`*` and an unbound port as `*`; an IPv6 address is bracketed so the
`:port` separator is unambiguous.

`ss` takes options only. The iproute2 filter-expression grammar (state
and address filters) is not implemented, so a bare operand is a usage
error rather than a silently ignored argument.

## OPTIONS

- `-t, --tcp` — show TCP sockets. With neither `-t` nor `-u`, both
  protocols are shown.
- `-u, --udp` — show UDP sockets.
- `-a, --all` — show both listening and connected sockets.
- `-l, --listening` — show only listening sockets.
- `-n, --numeric` — do not resolve service names. Always in force on
  TAIRiX; accepted for familiarity.
- `-p, --processes` — add the owning-process column (`pid=N`).
- `-4, --ipv4` — restrict the listing to IPv4 sockets.
- `-6, --ipv6` — restrict the listing to IPv6 sockets.
- `-H, --no-header` — suppress the header line.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `ss` — the connected, non-listening sockets.
- `ss -a` — every socket, listening and connected.
- `ss -l` — only the listening sockets.
- `ss -tlp` — listening TCP sockets, with the owning process.
- `ss -u4` — the UDP sockets over IPv4.

## EXIT STATUS

- `0` — the listing was produced (or the short help was written).
- `1` — the socket query was refused or failed, or the output could not
  be written.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
