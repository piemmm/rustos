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
boot-time consumer (the login service's `os.loginType`) reads through, so
producer and consumer can never diverge. Settings take effect at the point
their consumer parses the store: the store lives on the encrypted root
volume, so it is read only after the `ARXFS passphrase:` unlock.

Reads and writes go through the secured VFS under the caller's own
kernel-attested identity: `/System/Settings` is owned by the system
principal, so listing works for anyone the per-inode policy admits to read,
while changing a setting requires a principal it admits to write — a
refused write is reported with its reason and changes nothing (fail
closed). The tool requests `CAP_CONSOLE_WRITE` (its output and short help)
and `CAP_FS_ACCESS` (the store and its own `Help/` tree), nothing else.
