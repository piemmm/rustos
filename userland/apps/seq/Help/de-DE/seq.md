## NAME

seq — eine Zahlenfolge ausgeben

## SYNOPSIS

`seq [-f format] [-s string] [-w] [anfang [schritt]] ende`

## DESCRIPTION

Gibt die Zahlen von `anfang` bis `ende` in Schritten von `schritt` aus,
standardmäßig eine pro Zeile. Ein ausgelassener `anfang` oder `schritt`
gilt als 1 — auch wenn `ende` kleiner als `anfang` ist, sodass
`seq 5 1` nichts ausgibt. Die Folge endet, wenn das Addieren von
`schritt` über `ende` hinausführen würde.

Alle drei Operanden werden als Gleitkommazahlen gelesen; `schritt` ist
üblicherweise positiv, wenn `anfang` unter `ende` liegt, und negativ im
umgekehrten Fall, und darf nicht null sein. `ende` darf `inf` sein, um
endlos zu zählen. Die Standard-Ausgabegenauigkeit folgt der Schreibweise
der Operanden (`seq 1 0.25 2` gibt zwei Nachkommastellen aus), und reine
Ganzzahlfolgen werden exakt erzeugt, gleich wie groß die Zahlen sind.

Die Optionsauswertung endet am ersten Operanden, und eine führende
negative Zahl ist ein Operand, keine Option: `seq -5 5` zählt ab -5.

## OPTIONS

- `-f, --format <format>` — jede Zahl über das printf-artige
  Gleitkomma-`<format>` ausgeben (genau eine `%`-Direktive vom Typ `e`,
  `f`, `g` oder `a`, groß oder klein, mit den üblichen Flags, Breite und
  Genauigkeit). Nicht mit `-w` kombinierbar.
- `-s, --separator <string>` — die Zahlen mit `<string>` statt eines
  Zeilenumbruchs trennen. Die Ausgabe endet weiterhin mit einem
  Zeilenumbruch.
- `-w, --equal-width` — jede Zahl mit führenden Nullen auf eine
  gemeinsame Breite auffüllen. Nicht mit `-f` kombinierbar.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.
- `--` — die Optionsauswertung beenden; jedes weitere Argument ist ein
  Operand.

## EXAMPLES

- `seq 5` — 1 bis 5 ausgeben.
- `seq 2 5` — 2 bis 5 ausgeben.
- `seq 1 2 10` — die ungeraden Zahlen von 1 bis 9 ausgeben.
- `seq 5 -1 1` — von 5 bis 1 herunterzählen.
- `seq -w 8 10` — `08`, `09`, `10` ausgeben.
- `seq -s , 3` — `1,2,3` ausgeben.
- `seq -f %.2f 3` — `1.00`, `2.00`, `3.00` ausgeben.

## EXIT STATUS

- `0` — die Folge (oder die angeforderte Kurzhilfe) wurde geschrieben.
- `1` — die Ausgabe nahm keine Bytes mehr an.
- `2` — die Befehlszeile wurde nicht verstanden (unbekannte Option,
  ungültige Zahl, Schrittweite null oder fehlerhaftes Format).

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `fr-FR`).

## SEE ALSO

- `yes`
- `man`
