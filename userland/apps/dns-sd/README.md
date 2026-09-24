# tairix-dns-sd

The `dns-sd` command app (`plans/ZEROCONF.md` Z4): browse the link for a
service type, resolve one instance to where it is reached, look up a `.local`
host, or enumerate every type the segment offers — each through `discoveryd`,
never by speaking multicast DNS itself.

It is an administrator's diagnostic: its manifest requests
`CAP_NET_DISCOVER_ALL`, which only an administrator's ceiling carries, and
without which the service grants nothing past a host lookup. Every name it
prints was authored by a peer on the segment, so each is printed in RFC 1035
presentation form, escaped, never as it arrived.

The pure parse-and-render engine is `src/lib.rs` (host-tested against a
scripted discovery seam); `src/run.rs` is the freestanding `Run` binary.
