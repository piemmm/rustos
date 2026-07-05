## NAME

cat — Dateien auf die Standardausgabe zusammenführen

## SYNOPSIS

`cat [-n] [--] [file...]`

## DESCRIPTION

Liest jeden Dateioperanden der Reihe nach und schreibt seine Bytes auf
die Standardausgabe. Der Operand `-` bezeichnet die Standardeingabe;
ohne Operand ist die Standardeingabe die einzige Quelle.

Mit `-n` werden die Ausgabezeilen fortlaufend über alle Quellen
nummeriert, sodass eine Zeile, die sich über zwei Quellen erstreckt,
genau einmal nummeriert wird — beim Erscheinen ihres ersten Bytes.

Eine Quelle, die nicht gelesen werden kann, beendet den Befehl, bevor
eine spätere Quelle berührt wird; bereits geschriebene Bytes bleiben
geschrieben.

## OPTIONS

- `-n, --number` — Ausgabezeilen nummerieren, fortlaufend über alle
  Quellen.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `cat notes.txt` — `notes.txt` auf die Standardausgabe schreiben.
- `cat a.txt - b.txt` — `a.txt`, dann die Standardeingabe, dann
  `b.txt` schreiben.
- `cat -n log.txt` — jede Ausgabezeile nummerieren.
- `cat -- -n` — die Datei namens `-n` schreiben.

## EXIT STATUS

- `0` — jede Quelle wurde geschrieben.
- `1` — eine Quelle konnte nicht gelesen oder die Ausgabe nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `ls`
- `man`
