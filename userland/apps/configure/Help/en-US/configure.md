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
boot's login).

The set of keys is closed: an unknown key, or a value outside a key's
set, is refused with the valid choices stated and changes nothing.
Changing a setting rewrites the store in its canonical form and
requires write access to `/System/Settings` — an ordinary account can
read the settings but not change them.

- `os.loginType` — `text` or `graphical`: which session type the login
  service starts for an authenticated user. `text` (the default) starts
  the account's shell — the desktop can still be started on demand with
  the `desktop` command; `graphical` starts the desktop session
  directly after authentication when a desktop is installed, degrading
  to text when none is.

## OPTIONS

- `-h, -?` — show this command's own short help.

## EXAMPLES

- `configure` — list every setting.
- `configure os.loginType` — show the boot-default session type.
- `configure os.loginType graphical` — boot to the graphical login.

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
