# configure

The TAIRiX boot-time configuration command (the `sysctl`-shaped settings
tool): list, show, and set the settings of the system-configuration store
at `/System/Settings/Configuration/system.conf`.

- `configure` — list every setting and its current value.
- `configure <key>` — show one setting's current value.
- `configure <key> <value>` — set one setting (e.g.
  `configure os.loginType graphical`).

The store's grammar, closed key registry, fail-closed parse, and canonical
render are the shared `lib/sysconfig` engine — the same engine every
boot-time consumer (the login service's `os.loginType`, the cache manager's
`cache.*`, the network stack's `net.*`) reads through, so producer and
consumer can never diverge. Settings take effect at the point their
consumer parses or applies the store: the store lives on the encrypted root
volume, so it is read only after the `ARXFS passphrase:` unlock.

The registry spans three families: `os.*` (the login default), `cache.*`
(the SMARTRAM caching switches), and `net.*` — the stack-wide network
knobs: `net.ipv4.enabled` / `net.ipv6.enabled` (the address-family
switches), `net.ipv6.privacy` (RFC 8981 temporary IPv6 addresses),
`net.tcp.syncookies` (`auto` / `always`, the SYN-flood defence — never an
`off`), and `net.tcp.keepalive` (RFC 9293 §3.8.4 TCP keepalive probing on
idle connections, off by default). Per-interface network configuration is a
separate declarative store
(`/System/Settings/Network/network.conf`, the `lib/netconfig` engine), not
part of this command.

Reads and writes go through the secured VFS under the caller's own
kernel-attested identity: `/System/Settings` is owned by the system
principal, so listing works for anyone the per-inode policy admits to read,
while changing a setting requires a principal it admits to write — a
refused write is reported with its reason and changes nothing (fail
closed). The tool requests `CAP_CONSOLE_WRITE` (its output and short help)
and `CAP_FS_ACCESS` (the store and its own `Help/` tree), nothing else.
