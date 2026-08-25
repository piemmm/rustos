## NAME

host — look up a name over DNS

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

Resolves a domain name to its addresses using the system's stub resolver and
prints each answer, one per line. With no `-t`, both the `A` (IPv4) and
`AAAA` (IPv6) records are looked up; `-t type` restricts the lookup to one.

The recursive DNS servers to query are read from the host configuration
through the System Information API — the same active set the
`state:net/resolver/servers` read reports — and every response is validated
before an address is shown. There is no `/etc/resolv.conf` and no local host
file.

An operand that is an IPv4 or IPv6 address literal is a **reverse** lookup:
it is rewritten to the `in-addr.arpa` / `ip6.arpa` name the address maps to,
the default record type becomes `PTR`, and a found record prints as
`<reverse-name> domain name pointer <name>.`

Only the `A`, `AAAA`, and `PTR` records are supported; other record types
(`MX`, `TXT`, and so on) are rejected rather than silently treated as `A`. A
name that does not exist prints `Host <name> not found: 3(NXDOMAIN)`; when no
server can be reached, `host` reports a timeout on standard error.

## OPTIONS

- `-t, --type` — the DNS record type to look up: `A`, `AAAA`, or `PTR`
  (case-insensitive). Without it, a name looks up both `A` and `AAAA`, and
  an address looks up `PTR`.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `host example.com` — the IPv4 and IPv6 addresses of the name.
- `host -t AAAA example.com` — only the IPv6 addresses.
- `host 10.0.2.2` — the name that address maps back to.

## EXIT STATUS

- `0` — at least one address was found (or the short help was written).
- `1` — the name resolved to no address (a negative answer, a timeout, or a
  resolver failure).
- `2` — the command line was not understood, or the output could not be
  written.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
