## NAME

printf — Daten formatieren und ausgeben

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Gibt die `argument`e unter der Kontrolle von `format` aus, wie die
C-Funktion `printf`. Das Format enthält drei Arten von Elementen:
gewöhnliche Zeichen, die auf die Standardausgabe kopiert werden;
Backslash-Escapes; und `%`-Umwandlungsdirektiven, von denen jede das
nächste Argument umwandelt.

Die Escapes sind `\a` (Signalton), `\b` (Rückschritt), `\c` (alle
Ausgabe sofort beenden), `\e` (Escape), `\f` (Seitenvorschub), `\n`
(Zeilenumbruch), `\r` (Wagenrücklauf), `\t` (Tabulator), `\v`
(vertikaler Tabulator), `\\`, `\"`, `\NNN` (ein bis drei Oktalziffern),
`\xHH` (eine oder zwei Hexziffern) sowie `\uHHHH` / `\UHHHHHHHH`
(Unicode-Codepunkte, vier oder acht Hexziffern).

Die Umwandlungen sind `%d`/`%i` (dezimal mit Vorzeichen), `%u` (dezimal
ohne Vorzeichen), `%o`/`%x`/`%X` (oktal und hexadezimal), `%e`/`%E`/
`%f`/`%F`/`%g`/`%G`/`%a`/`%A` (Gleitkomma), `%c` (das erste Zeichen des
Arguments), `%s` (Zeichenkette), `%b` (Zeichenkette, deren eigene
Escapes interpretiert werden, oktal als `\0NNN`), `%q` (Zeichenkette,
für die Wiederverwendung in einer Shell zitiert) und `%%` (ein
wörtliches `%`). Eine Direktive akzeptiert die C-Flags `-`, `+`,
Leerzeichen, `#`, `0` und `'`, eine Feldbreite und eine Genauigkeit;
Breite und Genauigkeit können jeweils `*` sein und lesen ihren Wert dann
aus dem nächsten Argument. `%b` und `%q` akzeptieren weder Flags noch
Breite noch Genauigkeit.

Das Format wird so oft wiederverwendet, bis jedes Argument verbraucht
ist; eine Umwandlung ohne verbleibendes Argument gibt Null oder die
leere Zeichenkette aus. Ein numerisches Argument wird wie eine C-Zahl
gelesen (`0x` hexadezimal, führende `0` oktal, Gleitkomma, `inf`,
`nan`); ein führendes `'` oder `"` wandelt den Codepunkt des folgenden
Zeichens um. Ein Argument, das keine, nur teilweise eine oder eine
bereichsüberschreitende Zahl ist, wird auf der Fehlerausgabe
diagnostiziert und so weit wie möglich umgewandelt — der Lauf geht
weiter und endet mit Status `1`. Eine unbekannte Umwandlung, ein Flag
auf einer Umwandlung, die es nicht akzeptiert, oder ein fehlerhaftes
Escape beendet den Lauf mit einer Diagnose.

Zwei bewusste Abweichungen vom GNU-`printf`: Gleitkomma wird in
IEEE-754-doppelter Genauigkeit berechnet (GNU nutzt `long double`),
sodass ein Wert jenseits des Double-Bereichs `inf` ausgibt; und ein
*erstes* Argument `-h` oder `-?` zeigt diese Kurzhilfe — ein solches
Format schreibt man `printf -- -h...`.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen (nur als erstes
  Argument).
- `--` — die Optionsanalyse beenden; das nächste Argument ist das
  Format.

## EXAMPLES

- `printf '%s\n' hello` — `hello` und einen Zeilenumbruch ausgeben.
- `printf '%d\n' 0x10` — `16` ausgeben.
- `printf '%5.2f|\n' 3.14159` — ` 3.14|` ausgeben.
- `printf '%s=%q\n' greeting 'hi there'` — `greeting='hi there'`
  ausgeben.
- `printf '%b' 'one\ntwo\n'` — zwei Zeilen aus einem Argument ausgeben.
- `printf '%s-' a b c` — das Format wiederverwenden: `a-b-c-`.

## EXIT STATUS

- `0` — alles (oder die angeforderte Kurzhilfe) wurde geschrieben.
- `1` — ein Umwandlungsproblem wurde diagnostiziert, das Format fehlte
  oder war ungültig, ein Escape war fehlerhaft, oder die Ausgabe nahm
  keine Bytes mehr an.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `seq`
- `man`
