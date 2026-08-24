## NAME

cat — Dateien auf die Standardausgabe verketten

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Liest jeden Datei-Operanden der Reihe nach und schreibt seine Bytes
auf die Standardausgabe. Der Operand `-` bezeichnet die
Standardeingabe; ohne Operand ist die Standardeingabe die einzige
Quelle.

Ein Operand kann auch eine typisierte Ressourcenreferenz wie
`sys:random` sein: Sie wird über den berechtigungsgeprüften
Ressourcen-Resolver des Systems geöffnet, nicht über das Dateisystem —
`cat sys:random` liefert Zufallsbytes. Eine `info:`-, `state:`- oder
`stats:`-Referenz benennt einen typisierten Systemwert statt eines
Datenstroms; er wird über den Systeminformationsdienst gelesen, sodass
`cat info:mem/physical` diesen Wert ausgibt und ein Lesevorgang ohne
Berechtigung mit Nennung der fehlenden Berechtigung abgelehnt wird.
Eine fehlerhafte Referenz in einem registrierten Namensraum ist ein
Fehler und fällt nie auf einen Dateinamen zurück.

Mit `-n` werden die Ausgabezeilen fortlaufend über alle Quellen
nummeriert, sodass eine Zeile, die sich über zwei Quellen erstreckt,
genau einmal nummeriert wird — beim Erscheinen ihres ersten Bytes.
`-b` nummeriert nur nicht-leere Zeilen und hat Vorrang vor `-n`.
`-s` unterdrückt wiederholte aufeinanderfolgende Leerzeilen; eine
unterdrückte Zeile wird weder geschrieben noch nummeriert.

Die Markierungsoptionen machen unsichtbare Bytes sichtbar: `-E` gibt
`$` vor jedem Zeilenumbruch aus, `-T` stellt TAB als `^I` dar, und
`-v` stellt andere Steuerbytes als `^X` und Nicht-ASCII-Bytes in
`M-`-Notation dar. `-e`, `-t` und `-A` sind die üblichen
Kombinationen `-vE`, `-vT` und `-vET`.

Eine Quelle, die nicht gelesen werden kann, stoppt den Befehl, bevor
eine spätere Quelle berührt wird; bereits geschriebene Bytes bleiben
geschrieben.

## OPTIONS

- `-A, --show-all` — gleichbedeutend mit `-vET`.
- `-b, --number-nonblank` — nicht-leere Ausgabezeilen nummerieren;
  hat Vorrang vor `-n`.
- `-e` — gleichbedeutend mit `-vE`.
- `-E, --show-ends` — `$` am Ende jeder Zeile ausgeben.
- `-n, --number` — Ausgabezeilen nummerieren, fortlaufend über alle
  Quellen.
- `-s, --squeeze-blank` — wiederholte aufeinanderfolgende Leerzeilen
  unterdrücken.
- `-t` — gleichbedeutend mit `-vT`.
- `-T, --show-tabs` — TAB-Zeichen als `^I` darstellen.
- `-u` — akzeptiert und ignoriert; die Ausgabe ist bereits
  ungepuffert.
- `-v, --show-nonprinting` — `^`- und `M-`-Notation für Steuer- und
  Nicht-ASCII-Bytes verwenden, außer Zeilenvorschub und TAB.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `cat notes.txt` — `notes.txt` auf die Standardausgabe schreiben.
- `cat a.txt - b.txt` — `a.txt`, dann die Standardeingabe, dann
  `b.txt` schreiben.
- `cat -n log.txt` — jede Ausgabezeile nummerieren.
- `cat -bs draft.txt` — die nicht-leeren Zeilen nummerieren und
  Leerzeilenfolgen zusammenfassen.
- `cat -A config.txt` — Zeilenenden, Tabulatoren und Steuerbytes
  sichtbar machen.
- `cat -- -n` — die Datei namens `-n` schreiben.

## EXIT STATUS

- `0` — jede Quelle wurde geschrieben.
- `1` — eine Quelle konnte nicht gelesen oder die Ausgabe nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `ls`
- `man`
