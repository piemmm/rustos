## NAME

ping — send ICMP echo requests to a network host

## SYNOPSIS

`ping [option...] address`

## DESCRIPTION

Sends ICMP (IPv4) or ICMPv6 (IPv6) echo requests to a host and reports
each reply and its round-trip time, then a closing statistics block.

The requests flow through an ICMP echo socket opened from the user-space
network stack, gated on `CAP_NET` and `CAP_NET_RAW` and audited. The
stack owns the echo identifier, so a socket only ever receives replies to
its own requests. There is no name resolution in this release, so the
target must be a literal IPv4 or IPv6 address; a hostname is a usage
error rather than a silent failure.

By default `ping` sends one request per second until interrupted; `-c`
bounds the count. Each reply prints the source, sequence number, and
time; a request with no reply within the timeout prints a timeout line.
The closing block reports the packets transmitted and received, the loss
percentage, and the minimum, average, and maximum round-trip times. `-q`
prints only the header and the statistics.

The IP time-to-live is not exposed through the echo-socket interface, so —
unlike some `ping` implementations — a reply line carries no `ttl=` field.

## OPTIONS

- `-c, --count` — stop after sending this many requests.
- `-i, --interval` — seconds between requests (a decimal, e.g. `0.5`).
- `-s, --size` — payload size in bytes.
- `-W, --timeout` — seconds to wait for each reply.
- `-w, --deadline` — overall run deadline in seconds.
- `-4, --ipv4` — require an IPv4 target.
- `-6, --ipv6` — require an IPv6 target.
- `-n, --numeric` — numeric output. Always in force on TAIRiX; accepted
  for familiarity.
- `-q, --quiet` — quiet: only the header and the final statistics.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `ping 10.0.2.2` — ping an IPv4 host until interrupted.
- `ping -c 4 fe80::1` — send four requests to an IPv6 host.
- `ping -c 10 -i 0.2 10.0.0.1` — ten requests, one every 200 ms.
- `ping -q -c 100 10.0.0.1` — a quiet run, summary only.

## EXIT STATUS

- `0` — at least one reply was received (or the short help was written).
- `1` — every request went unanswered.
- `2` — the command line was not understood, or the socket could not be
  opened.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
