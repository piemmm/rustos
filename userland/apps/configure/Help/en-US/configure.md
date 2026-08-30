## NAME

configure — read and set the boot-time system configuration

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Lists, shows, and sets the settings of the system-configuration store
at `/System/Settings/Configuration/system.conf`. With no operand every
setting is listed with its current value; with a key alone that
setting's value is shown; with a key and a value the setting is
changed.

The store lives on the encrypted root volume and is parsed by its
consumers after the root filesystem is unlocked, so a change takes
effect the next time its consumer starts (`os.loginType`: the next
boot's login; the `cache.*` switches: the next boot's unlock).

The set of keys is closed: an unknown key, or a value outside a key's
set, is refused with the valid choices stated and changes nothing.
Changing a setting rewrites the store in its canonical form and
requires write access to `/System/Settings` — an ordinary account can
read the settings but not change them.

- `os.loginType` — `text` or `graphical`: which session type the login
  service starts for an authenticated user. `graphical` (the default)
  starts the desktop session directly after authentication, degrading
  to the text login on a machine that cannot run one; `text` starts the
  account's shell — the desktop can still be started on demand with the
  `desktop` command.
- `cache.all` — `on` or `off`: the master caching switch. `on` (the
  default) lets each cache class below follow its own setting; `off`
  is a ceiling that disables every memory cache regardless of the
  per-class settings.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` or `off`: the per-class switches for the
  four reclaimable memory caches (the filesystem, whole-disk block,
  decompressed-cluster, and application-launch caches). `auto` (the
  default) lets the memory-pressure manager govern the class; `off`
  disables it outright. There is no per-class `on`: a class cannot be
  forced to ignore memory pressure. A class is effectively `off`
  whenever `cache.all` is `off`.

Every cache is a reclaimable accelerator, never the source of truth, so
turning any or all of them off only makes the affected work slower — it
never changes a result.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` or `false`: the
  stack-wide address-family switches. Both are `true` by default. A
  disabled family binds no addresses, answers no packets, and refuses
  a socket in that family with a typed error — never a silent drop.
- `net.ipv6.privacy` — `true` or `false`: whether the stack forms
  temporary (privacy) IPv6 addresses alongside the stable one. `false`
  (the default) uses the stable SLAAC address only.
- `net.tcp.syncookies` — `auto` or `always`: the SYN-flood defence
  policy. `auto` (the default) keeps a bounded half-open queue and
  falls back to stateless cookies on overflow; `always` answers every
  connection request statelessly. There is no `off` — an undefended
  connection queue is never a setting.
- `net.tcp.keepalive` — `true` or `false`: whether TCP connections
  send keepalive probes on an idle link. `false` (the default) never
  probes and never tears an idle connection down for inactivity;
  `true` probes an idle peer after the standard idle interval and
  drops the connection if the peer stops answering.
- `net.tcp.ecn` — `true` or `false`: whether TCP connections negotiate
  Explicit Congestion Notification. `false` (the default) leaves
  connections Not-ECT; `true` offers ECN in the handshake and, once
  negotiated, treats a congestion mark as a signal to slow down instead
  of forcing a packet drop.
- `time.servers` — `none` or a comma-separated list of network time
  servers, each a host name or an address literal. `none` (the default)
  means the clock is never set from the network: TAIRiX has no time-server
  pool of its own, so naming a server is the operator's choice rather than
  a default aimed at somebody else's service.
- `time.refresh` — `6h`, `12h`, `1d`, `2d`, or `7d`: how much uptime
  passes between clock re-queries once the time is known. `1d` is the
  default. A clock that is unset, implausible, or long stale is corrected
  as soon as the network allows, whatever this says.

Changing a `net.*` setting saves it and delivers it to the running network
stack, so it takes effect at once. If the running stack does not accept it
— none is running, or your account may not administer the network — the
setting is still saved and `configure` says so; it then applies at the next
boot.

## OPTIONS

- `-h, -?` — show this command's own short help.

## EXAMPLES

- `configure` — list every setting.
- `configure os.loginType` — show the boot-default session type.
- `configure os.loginType graphical` — boot to the graphical login.
- `configure cache.all off` — disable every memory cache system-wide.
- `configure cache.filesystem off` — disable only the filesystem cache.
- `configure net.ipv6.enabled false` — turn IPv6 off stack-wide.
- `configure time.servers 0.example.test,1.example.test` — set the
  network time servers the clock is synchronised from.

## EXIT STATUS

- `0` — the listing, value, short help, or change was completed.
- `1` — the store could not be read or written (for example the
  caller may not change system settings), or the output could not be
  delivered.
- `2` — the command line was not understood, the key is unknown, or
  the value is outside the key's set.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
