## NAME

rmdir — leere Verzeichnisse entfernen

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] Verzeichnis...`

## DESCRIPTION

Entfernt jeden Verzeichnis-Operanden der Reihe nach. Nur ein **leeres
Verzeichnis** wird entfernt: das Dateisystem selbst weist eine Datei
(oder jedes andere Objekt) und ein nicht leeres Verzeichnis atomar
zurück, sodass niemals etwas anderes an seiner Stelle entfernt werden
kann. Für Dateien dient `rm`, für gefüllte Bäume `rm -r`.

Mit `-p` werden auch die Vorfahren jedes Operanden entfernt, vom
innersten zum äußersten: `rmdir -p a/b/c` entfernt `a/b/c`, dann
`a/b`, dann `a`. Die nackte Wurzel eines Pfads (`/` oder eine
Alias-Wurzel wie `Home:/`) wird nie angefragt.

Mit `--ignore-fail-on-non-empty` ist die Zurückweisung „Verzeichnis
nicht leer" kein Fehler — der Operand (oder der `-p`-Aufstieg) endet
einfach dort. Keine andere Zurückweisung wird toleriert. Der erste
echte Fehlschlag beendet den Lauf vor jedem weiteren Operanden. `--`
beendet die Optionsanalyse: jedes spätere Argument ist ein Pfad.

## OPTIONS

- `-p, --parents` — auch die Vorfahren jedes Operanden entfernen, vom
  innersten zum äußersten.
- `-v, --verbose` — jeden Entfernungsversuch als
  `rmdir: removing directory, 'Verz'` melden.
- `--ignore-fail-on-non-empty` — ein nicht leeres Verzeichnis ist kein
  Fehler; mit `-p` endet der Aufstieg dort.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen (auch `--help`).

## EXAMPLES

- `rmdir Scratch` — ein leeres Verzeichnis entfernen.
- `rmdir -p Projects/os/build` — die Kette entfernen, vom innersten
  zum äußersten.
- `rmdir -p --ignore-fail-on-non-empty a/b` — `a/b` entfernen, und
  auch `a`, wenn es dadurch leer wird.

## EXIT STATUS

- `0` — jede Entfernung gelang (eine von `--ignore-fail-on-non-empty`
  tolerierte Zurückweisung ist kein Fehlschlag).
- `1` — ein Dateisystem- oder Ausgabefehler; der Grund wird auf der
  Standardfehlerausgabe gemeldet.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

mkdir, rm, ls
