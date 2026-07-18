## NAME

tail — den letzten Teil von Dateien ausgeben

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Gibt die letzten 10 Zeilen jeder `file` auf der Standardausgabe aus. Bei
mehreren `file` wird jedem Teil eine Kopfzeile `==> file <==`
vorangestellt. Ohne `file`, oder wenn `file` gleich `-` ist, wird die
Standardeingabe gelesen.

`-n` und `-c` ändern die ausgegebene Menge: eine einfache Anzahl (oder
eine mit führendem `-`) gibt die letzten `num` Zeilen oder Bytes aus;
eine Anzahl mit führendem `+` gibt alles **ab** Zeile oder Byte `num`
(gezählt ab 1) bis zum Ende aus. Eine Anzahl kann ein
Multiplikatorsuffix tragen: `b` (512), `kB` (1000), `K` (1024), `MB`,
`M`, `GB`, `G` und so weiter für `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (ein
einzelner Buchstabe multipliziert mit Potenzen von 1024; mit `B` mit
Potenzen von 1000; mit `iB` mit Potenzen von 1024).

Die historische Erst-Argument-Form `tail -num` / `tail +num` (mit einem
optionalen abschließenden Buchstaben `b`/`c`/`l`) wird akzeptiert, wie im
GNU-Werkzeug.

Der Folgemodus (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`) ist noch nicht verfügbar
und wird als unbekannte Option gemeldet: er benötigt eine
Aufweck-Quelle bei Dateiänderungen, die das System noch nicht bietet,
und es wird kein aktives Warten an ihrer Stelle geliefert.

Wenn führender Inhalt nicht angezeigt wird, wird ein Hinweisdatensatz
auf den Standard-Informationsstrom (fd 3) geschrieben; er ändert nie die
Ausgabe oder den Exit-Status. Eine nicht lesbare Datei wird auf der
Standardfehlerausgabe gemeldet und der Lauf fährt mit der nächsten Datei
fort.

## OPTIONS

- `-c, --bytes <num>` — die letzten `num` Bytes jeder Datei ausgeben;
  mit führendem `+` alles ab Byte `num`.
- `-n, --lines <num>` — die letzten `num` Zeilen jeder Datei ausgeben;
  mit führendem `+` alles ab Zeile `num`.
- `-q, --quiet, --silent` — die Kopfzeilen `==> file <==` nie ausgeben.
- `-v, --verbose` — die Kopfzeilen `==> file <==` immer ausgeben.
- `-z, --zero-terminated` — Zeilen sind NUL-getrennt statt
  zeilenumbruchgetrennt.
- `-h, -?` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `tail log.txt` — die letzten 10 Zeilen von `log.txt` ausgeben.
- `tail -n 3 a b` — die letzten 3 Zeilen von `a` und `b` ausgeben, je
  unter ihrer Kopfzeile.
- `tail -c 1K image` — die letzten 1024 Bytes von `image` ausgeben.
- `tail -n +5 notes` — `notes` ab der 5. Zeile ausgeben.

## EXIT STATUS

- `0` — jede Datei wurde ausgegeben (oder die Kurzhilfe wurde
  geschrieben).
- `1` — eine Datei konnte nicht gelesen werden, oder die Ausgabe konnte
  nicht geliefert werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
