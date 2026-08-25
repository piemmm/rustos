## NAME

ping — send ICMP echo requests to a network host

## SYNOPSIS

`ping [option...] host`

## DESCRIPTION

Sends ICMP (IPv4) or ICMPv6 (IPv6) echo requests to a host and reports
each reply and its round-trip time, then a closing statistics block.

The requests flow through an ICMP echo socket opened from the user-space
network stack, gated on `CAP_NET` and `CAP_NET_RAW` and audited. The
stack owns the echo identifier, so a socket only ever receives replies to
its own requests.

The target is an IPv4 or IPv6 address literal or a host name. A name is
resolved through the system stub resolver, using the recursive servers the
host is configured with; a literal needs no query, so it works even where
no resolver is configured. A name that does not resolve to an address of
the wanted family ends the run with its reason.

Each request carries high-entropy random data by default, drawn fresh for
every request. This is deliberate: a link that compresses or de-duplicates
traffic would otherwise report a throughput and latency that say nothing
about its real capacity. The echoed bytes are compared with what was sent,
so a random payload is also a per-packet integrity check. Use `-p` for a
fixed pattern when a deterministic payload is what is wanted.

By default `ping` sends one request per second until interrupted; `-c`
bounds the count. Each reply prints the source, sequence number, and
time; a request with no reply within the timeout prints a timeout line.
The closing block reports the packets transmitted and received, the loss
percentage, and the minimum, average, and maximum round-trip times. `-q`
prints only the header and the statistics.

Each reply names the peer as `name (address)` when the address has a
`PTR` record, resolved once for the run through the same stub resolver;
an address with no name, and every run under `-n`, prints the bare
address. `-n` also means no `PTR` query is put on the wire at all.

The IP time-to-live is not exposed through the echo-socket interface, so —
unlike some `ping` implementations — a reply line carries no `ttl=` field.

## OPTIONS

- `-c, --count` — stop after sending this many requests.
- `-i, --interval` — seconds between requests (a decimal, e.g. `0.5`).
- `-s, --size` — payload size in bytes.
- `-p, --pattern` — payload contents: `random` (the default,
  high-entropy) or an even-length string of hex digits giving a repeating
  byte pattern, e.g. `-p ff00`.
- `-W, --timeout` — seconds to wait for each reply.
- `-w, --deadline` — overall run deadline in seconds.
- `-4, --ipv4` — require an IPv4 target.
- `-6, --ipv6` — require an IPv6 target.
- `-n, --numeric` — numeric output: do not reverse-resolve the peer, so
  no `PTR` query is made and reply lines carry the bare address.
- `-q, --quiet` — quiet: only the header and the final statistics.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `ping 10.0.2.2` — ping an IPv4 host until interrupted.
- `ping gateway.example` — ping a host by name.
- `ping -c 4 fe80::1` — send four requests to an IPv6 host.
- `ping -c 10 -i 0.2 10.0.0.1` — ten requests, one every 200 ms.
- `ping -q -c 100 10.0.0.1` — a quiet run, summary only.
- `ping -s 1400 -c 20 host.example` — twenty large, incompressible
  requests: the shape that measures a link honestly.
- `ping -p ff00 -c 4 10.0.0.1` — a fixed alternating pattern instead.

## EXIT STATUS

- `0` — at least one reply was received (or the short help was written).
- `1` — every request went unanswered.
- `2` — the command line was not understood, the target did not resolve,
  or the socket could not be opened.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `host`
- `ss`
- `sysinfo`
- `man`
