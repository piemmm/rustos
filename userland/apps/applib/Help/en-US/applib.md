## NAME

applib — administer the desktop's program library

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Administers the program library — the folder-organised catalog of
launchable applications the desktop's launcher presents. The library is
data on the volume, never a compiled-in list: a machine-wide store at
`/System/Settings/ProgramLibrary/library.conf` that every account reads,
plus an optional per-user overlay at the same path inside the user's own
`Settings/`. What a launcher shows is the two resolved together: the
user's own entries and adjustments win over the machine-wide ones.

With no subcommand (or `list`) the resolved library is printed folder by
folder, one entry per line: identifier, display name, and bundle path —
exactly what the launcher shows. The folders are the closed set
Accessories, Graphics, Internet, Multimedia, Office, Programming, Games,
SystemTools, Utilities, and Other; there is no free-form folder.

`applib add` registers an application bundle. Its identity, display name,
folder, and icon are taken from the bundle's own signed `AppInfo`
manifest; `--category`, `--name`, and `--icon` override the manifest. A
bundle whose manifest declares no library folder needs an explicit
`--category` — the tool never guesses. `applib remove` drops a record,
named by its identifier or by the bundle path it was registered with.

`applib hide` suppresses an entry from the resolved library without
removing its record — its identifier stays claimed, so a later `rescan`
cannot resurrect it — and `applib show` re-shows it. Hiding is
presentation, never authority: launching a bundle is still governed by
the loader's signature and capability checks regardless of the catalog.

`applib rescan` walks the application stores (`/System/Apps` and `/Apps`,
or the caller's own `<home>/Apps` under `--user`), reads each bundle's
manifest, and registers every application that asks to be listed and is
not yet catalogued. Existing records — including a curator's renames and
suppressions — are never disturbed, and a bundle with an unreadable or
malformed manifest is skipped and counted, never a reason to abort. This
is how a fresh system's library populates itself from the bundles
actually installed, with no hand-maintained list anywhere.

By default the tool edits the machine-wide store, which only a principal
admitted by the `/System/Settings` write policy can change; an ordinary
account reads it but personalises through its own overlay with `--user`.
A refused write states its reason and changes nothing.

On success the tool is quiet on standard output; the outcome of a change
is emitted as a structured advisory record on the standard information
stream (fd 3), which scripts may capture with `3>records.jsonl` and
everything else may ignore.

## OPTIONS

- `--category <folder>` — with `list`, show only that folder; with `add`,
  file the entry under it (overriding the manifest's declaration).
- `--name <name>` — with `add`, the display name to show instead of the
  manifest's.
- `--icon <asset>` — with `add`, the icon asset (a file name inside the
  bundle's own `Resources/`) instead of the manifest's.
- `--user` — apply the change to the caller's own overlay (or, with
  `rescan`, walk the caller's own `<home>/Apps`) instead of the
  machine-wide store.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `applib` — show the resolved library, folder by folder.
- `applib list --category Games` — show one folder.
- `applib add /Apps/chess.app` — register a bundle as its manifest asks.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  register a bundle that declares no listing, under an explicit folder.
- `applib remove os.tairix.chess` — drop an entry by identifier.
- `applib hide os.tairix.chess --user` — hide it from your own library
  only.
- `applib rescan` — register every installed, listed bundle not yet in
  the machine catalog.

## EXIT STATUS

- `0` — the listing, change, rescan, or short help was completed.
- `1` — a store, bundle, or output failure (for example the caller may
  not change the machine-wide catalog); the reason is stated on the
  diagnostic stream.
- `2` — the command line was not understood, the folder or entry is
  unknown, or the bundle cannot be registered as asked.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).
- `HOME` — the caller's home directory: names the per-user overlay and
  the `--user` rescan root `<home>/Apps`.

## SEE ALSO

- `man`
- `configure`
