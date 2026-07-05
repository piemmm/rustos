## NAME

mv — Dateien und Verzeichnisse verschieben (umbenennen)

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Verschiebt jeden Quelloperanden zu einem Ziel. Bei einer einzigen
Quelle und einem Ziel, das kein Verzeichnis benennt, wird die Quelle
auf genau diesen Pfad umbenannt. Benennt das Ziel ein bestehendes
Verzeichnis — und immer bei mehr als einer Quelle — wird jede Quelle
unter ihrem eigenen Basisnamen *in* dieses Verzeichnis verschoben.

Ein Verschieben innerhalb eines Volumes ist ein atomares Umbenennen,
das die Identität des Knotens bewahrt. Liegen Quelle und Ziel auf
verschiedenen Volumes, kann es nicht atomar sein: es fällt auf das
Kopieren der Quelle zum Ziel und das anschließende Entfernen der
Quelle zurück (Verzeichnisse werden rekursiv nachgebildet).

Ein bestehendes Ziel wird standardmäßig überschrieben, mit `-n`
übersprungen und mit `-i` über die Standardfehlerausgabe erfragt
(eine abgelehnte Frage überspringt dieses Verschieben ohne Fehler;
eine unlesbare Antwort gilt nie als Zustimmung). Der erste Fehlschlag
beendet den Lauf vor jedem späteren Operanden. `--` beendet die
Optionsauswertung: jedes spätere Argument ist ein Pfad.

## OPTIONS

- `-f, --force` — ein blockierendes Ziel entfernen und das Umbenennen
  wiederholen; nie fragen. Das spätere von `-f`, `-i` und `-n`
  gewinnt.
- `-i, --interactive` — vor dem Überschreiben eines bestehenden Ziels
  fragen; nur eine mit `y`/`Y` beginnende Antwort stimmt zu.
- `-n, --no-clobber` — ein bestehendes Ziel nie überschreiben.
- `-v, --verbose` — jedes Verschieben als `renamed 'source' -> 'dest'`
  melden.
- `-t dir, --target-directory=dir` — jede Quelle nach `dir`
  verschieben, das ein bestehendes Verzeichnis sein muss. Der Wert
  folgt angehängt (`-tdir`, `--target-directory=dir`) oder als
  nächstes Argument.
- `-T, --no-target-directory` — das Ziel als gewöhnliche Datei
  behandeln; genau eine Quelle ist erlaubt. Nicht mit `-t`
  kombinierbar.
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `mv draft.txt final.txt` — eine Datei umbenennen.
- `mv -v a.txt b.txt Archive` — beide Dateien nach `Archive`
  verschieben und jedes Verschieben melden.
- `mv -n new.cfg current.cfg` — eine Datei nur installieren, wenn das
  Ziel noch nicht existiert.

## EXIT STATUS

- `0` — jedes Verschieben ist gelungen (ein `-n`-Überspringen und
  eine abgelehnte `-i`-Frage sind keine Fehlschläge).
- `1` — ein Dateisystem-, Frage- oder Ausgabefehler; der Grund wird
  auf der Standardfehlerausgabe gemeldet.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
