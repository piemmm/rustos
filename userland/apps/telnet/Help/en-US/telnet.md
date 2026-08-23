## NAME

telnet — the RFC 854 Network Virtual Terminal client

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Opens a TCP connection to a host and relays the terminal to it: the host's
output appears on standard output, keystrokes go to the host, and the escape
character (`^]` by default) drops into the `telnet>` command interpreter.
With no host, `telnet` starts at that prompt and `open` connects.

It is both the way to reach a line-oriented service on another machine and
the way to poke any TCP service by hand — `telnet host 80` opens a
connection you can type a request into.

The host may be a name or a literal IPv4/IPv6 address. A name is resolved
through the system's stub resolver, which reads the configured recursive DNS
servers from the System Information API. The port is a number: there is no
services database, so a service *name* is a usage error rather than a silent
fall back to port 23.

Option negotiation follows RFC 855 with the RFC 1143 loop-free discipline,
so a peer that repeats itself never makes the client repeat itself. The
options this client implements are BINARY, ECHO, SUPPRESS GO AHEAD, STATUS,
TIMING MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL,
LINEMODE and NEW-ENVIRON; anything else is refused, which is what an
unimplemented option means, and a subnegotiation is honoured only for an
option that is actually enabled. RFC 1184 LINEMODE is implemented in full — the
`MODE` mask, the Set Local Characters table and `FORWARDMASK` — so the
client does the line editing the server asks it to, with the characters the
server negotiates.

The terminal window size is reported over NAWS when the connection is made
and again when it changes. TAIRiX has no window-change signal, so the size
is re-read whenever you type; a resize therefore reaches the host at your
next keystroke.

`NEW-ENVIRON` discloses **only** variables you define and export with the
`environ` command; the client never sends its own environment. `-a` and `-l`
export a login name, and that is the one thing an invocation discloses by
itself.

Where this client differs from the historical tool, it differs deliberately.
There is no `!` shell escape: a program that parses hostile network input is
not given the authority to spawn a shell. There is no `slc check`, because
RFC 1184 gives it no wire form distinct from `slc export`. TCP urgent data is
not exposed by the socket interface, so a Synch travels as the Data Mark
alone. And when standard input reaches end of file — a piped invocation such
as `telnet host 80 < request` — the write side is closed and the session keeps
reading until the remote host closes too, so the response is not discarded as
the historical tool discards it.

## OPTIONS

- `-4, --ipv4` — connect over IPv4 only.
- `-6, --ipv6` — connect over IPv6 only.
- `-8, --binary` — request an 8-bit data path in both directions.
- `-L, --eight-bit-output` — request an 8-bit data path on output only.
- `-E, --no-escape` — no escape character; every keystroke goes to the host.
- `-e, --escape <char>` — set the escape character (`^]`, `^A`, a single
  character, or empty for none).
- `-a, --login` — export the session's login name over `NEW-ENVIRON`.
- `-l, --user <name>` — export `name` as the login name (implies `-a`).
- `-b, --bind <address>` — bind this local address before connecting.
- `-d, --debug` — trace option negotiation on standard error.
- `-?, --help` — show this command's own short help.

## EXAMPLES

- `telnet example.test` — open a session on the assigned telnet port.
- `telnet 10.0.2.2 25` — speak to a mail service by hand.
- `telnet -6 fe80::2` — connect over IPv6 only.
- `telnet -l ada host` — offer `ada` as the login name.
- `telnet -8 host` — ask for an 8-bit path in both directions.
- `telnet` then `open host` — connect from the command prompt.

## EXIT STATUS

- `0` — the session ran (however the remote host ended it), or the short
  help was written.
- `1` — the session could not be had: the host would not resolve, the socket
  was refused, or the terminal could not be put into raw mode.
- `2` — the command line was not understood.

## ENVIRONMENT

- `TERM` — reported to the host over the TERMINAL TYPE option.
- `USER` — the login name `-a` exports.
- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
