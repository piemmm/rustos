## NAME

wc — Zeilen-, Wort- und Bytezahlen für jede Datei ausgeben

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Zählt für jede `file` ihre Zeilen (Zeilenumbruch-Zeichen), Wörter und
Bytes und gibt sie in einer Zeile aus, gefolgt vom Dateinamen. Ohne
`file`, oder wenn `file` gleich `-` ist, wird die Standardeingabe
gelesen (und für die Form ohne Operanden wird kein Name ausgegeben).
Bei mehr als einer Eingabe wird eine abschließende `total`-Zeile gemäß
`--total` ausgegeben.

Die Selektoren `-l`, `-w`, `-m`, `-c` und `-L` wählen die ausgegebenen
Zählungen; ohne einen davon werden Zeilen-, Wort- und Bytezahlen
ausgegeben. Zählungen erscheinen immer in der festen Reihenfolge:
Zeilen, Wörter, Zeichen, Bytes, maximale Zeilenbreite. Ein Wort ist
eine maximale Folge von Nicht-Leerraum-Zeichen. `-m` zählt
UTF-8-Zeichen (ein Byte, das kein gültiges UTF-8 ist, zählt als Byte,
aber nicht als Zeichen); `-L` misst die Anzeigebreite jeder Zeile in
Terminalspalten, wobei Tabulatoren zum nächsten Vielfachen von 8
vorrücken.

`--files0-from <file>` liest die NUL-getrennte Operandenliste aus
`file` (`-` bedeutet die Standardeingabe); sie kann nicht mit
`file`-Operanden kombiniert werden.

Eine unlesbare Eingabe wird auf der Standardfehlerausgabe gemeldet,
und der Lauf fährt mit der nächsten Eingabe fort.

## OPTIONS

- `-c, --bytes` — die Bytezahl ausgeben.
- `-m, --chars` — die Zeichenzahl ausgeben.
- `-l, --lines` — die Zeilenumbruchzahl ausgeben.
- `-w, --words` — die Wortzahl ausgeben.
- `-L, --max-line-length` — die maximale Anzeigebreite einer Zeile
  ausgeben.
- `--files0-from <file>` — die NUL-getrennte Operandenliste aus `file`
  lesen (`-` liest sie aus der Standardeingabe).
- `--total <when>` — wann die `total`-Zeile ausgegeben wird: `auto`
  (Voreinstellung: nur bei mehreren Eingaben), `always`, `only` (nur
  das Total, ohne Beschriftung) oder `never`.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `wc notes.txt` — Zeilen-, Wort- und Bytezahlen von `notes.txt`
  ausgeben.
- `wc -l a b` — die Zeilenzahl von `a` und von `b` ausgeben, dann das
  Total.
- `wc -L table.txt` — die breiteste Zeile von `table.txt` in
  Terminalspalten ausgeben.
- `wc -c --total=only a b` — nur die summierte Bytezahl ausgeben.

## EXIT STATUS

- `0` — jede Eingabe wurde gezählt (oder die Kurzhilfe wurde
  geschrieben).
- `1` — eine Eingabe konnte nicht gelesen oder die Ausgabe nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `cat`
- `head`
- `man`
