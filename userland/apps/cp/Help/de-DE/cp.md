## NAME

cp — Dateien und Verzeichnisse kopieren

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Kopiert jeden Quelloperanden zu einem Ziel. Bei einer einzigen Quelle
und einem Ziel, das kein Verzeichnis benennt, wird die Quelle auf
genau diesen Pfad kopiert. Benennt das Ziel ein bestehendes
Verzeichnis — und immer bei mehr als einer Quelle — wird jede Quelle
unter ihrem eigenen Basisnamen *in* dieses Verzeichnis kopiert.

Eine Verzeichnisquelle wird nur mit `-r` kopiert, das den ganzen
Teilbaum nachbildet; ohne `-r` wird ein Verzeichnisoperand abgelehnt.
Eine bestehende Zieldatei wird standardmäßig überschrieben, mit `-n`
übersprungen und mit `-i` über die Standardfehlerausgabe erfragt (eine
abgelehnte Frage überspringt diese Kopie ohne Fehler; eine unlesbare
Antwort gilt nie als Zustimmung).

Der erste Fehlschlag beendet den Lauf vor jedem späteren Operanden.
`--` beendet die Optionsauswertung: jedes spätere Argument ist ein
Pfad.

## OPTIONS

- `-r, -R, --recursive` — Verzeichnisse mitsamt Inhalt kopieren.
- `-f, --force` — wenn eine Zieldatei nicht angelegt werden kann, sie
  entfernen und die Kopie einmal wiederholen.
- `-i, --interactive` — vor dem Überschreiben einer bestehenden Datei
  fragen; nur eine mit `y`/`Y` beginnende Antwort stimmt zu.
- `-n, --no-clobber` — eine bestehende Datei nie überschreiben. Das
  spätere von `-i` und `-n` gewinnt.
- `-v, --verbose` — jede Kopie als `'source' -> 'dest'` melden.
- `-t dir, --target-directory=dir` — jede Quelle nach `dir` kopieren,
  das ein bestehendes Verzeichnis sein muss. Der Wert folgt angehängt
  (`-tdir`, `--target-directory=dir`) oder als nächstes Argument.
- `-T, --no-target-directory` — das Ziel als gewöhnliche Datei
  behandeln; genau eine Quelle ist erlaubt. Nicht mit `-t`
  kombinierbar.
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `cp notes.txt backup.txt` — eine Datei unter neuem Namen kopieren.
- `cp -r Projects Archive` — den Baum `Projects` in `Archive`
  nachbilden (oder als `Archive`, wenn es nicht existiert).
- `cp -v -t Backup a.txt b.txt` — beide Dateien nach `Backup`
  kopieren und jede Kopie melden.

## EXIT STATUS

- `0` — jede Kopie ist gelungen (ein `-n`-Überspringen und eine
  abgelehnte `-i`-Frage sind keine Fehlschläge).
- `1` — ein Dateisystem-, Frage- oder Ausgabefehler; der Grund wird
  auf der Standardfehlerausgabe gemeldet.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
