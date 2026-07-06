## NAME

head — den Anfang von Dateien ausgeben

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Gibt die ersten 10 Zeilen jeder `file` auf der Standardausgabe aus. Bei
mehreren `file` wird jedem Teil eine Kopfzeile `==> file <==`
vorangestellt. Ohne `file`, oder wenn `file` gleich `-` ist, wird die
Standardeingabe gelesen.

`-n` und `-c` ändern die ausgegebene Menge: eine einfache Zahl gibt die
ersten `num` Zeilen oder Bytes aus; eine Zahl mit führendem `-` gibt
alles **außer** den letzten `num` Zeilen oder Bytes aus. Eine Zahl darf
ein Multiplikator-Suffix tragen: `b` (512), `kB` (1000), `K` (1024),
`MB`, `M`, `GB`, `G` und so weiter für `T`, `P`, `E`, `Z`, `Y`, `R`,
`Q` (ein einzelner Buchstabe multipliziert mit Potenzen von 1024; mit
`B` mit Potenzen von 1000; mit `iB` mit Potenzen von 1024).

Die historische Erste-Argument-Form `head -num` (mit optionalen
`b`/`k`/`m`-Multiplikatoren und den Buchstaben `l`/`q`/`v`/`z`) wird
wie im GNU-Werkzeug akzeptiert.

Eine unlesbare Datei wird auf der Standardfehlerausgabe gemeldet, und
der Lauf fährt mit der nächsten Datei fort.

## OPTIONS

- `-c, --bytes <num>` — die ersten `num` Bytes jeder Datei ausgeben;
  mit führendem `-` alles außer den letzten `num` Bytes.
- `-n, --lines <num>` — die ersten `num` Zeilen jeder Datei ausgeben;
  mit führendem `-` alles außer den letzten `num` Zeilen.
- `-q, --quiet, --silent` — die Kopfzeilen `==> file <==` nie ausgeben.
- `-v, --verbose` — die Kopfzeilen `==> file <==` immer ausgeben.
- `-z, --zero-terminated` — Zeilen sind NUL-getrennt statt durch
  Zeilenumbruch.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `head log.txt` — die ersten 10 Zeilen von `log.txt` ausgeben.
- `head -n 3 a b` — die ersten 3 Zeilen von `a` und von `b` ausgeben,
  jeweils unter ihrer Kopfzeile.
- `head -c 1K image` — die ersten 1024 Bytes von `image` ausgeben.
- `head -n -1 notes` — `notes` ohne seine letzte Zeile ausgeben.

## EXIT STATUS

- `0` — jede Datei wurde ausgegeben (oder die Kurzhilfe wurde
  geschrieben).
- `1` — eine Datei konnte nicht gelesen oder die Ausgabe nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `cat`
- `wc`
- `man`
