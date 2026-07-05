## NAME

yes — eine Textzeile wiederholt ausgeben

## SYNOPSIS

`yes [Zeichenkette...]`

## DESCRIPTION

Schreibt seine Operanden, durch einzelne Leerzeichen verbunden — oder
`y`, wenn keine angegeben sind —, gefolgt von einem Zeilenumbruch,
immer wieder, bis die Ausgabe keine Bytes mehr annimmt (eine
geschlossene Pipe) oder der Prozess beendet wird. Seine historische
Aufgabe ist es, einem nachfragenden Befehl eine bejahende Antwort zu
liefern; seine moderne, eine günstige Quelle wiederholten Texts zu sein.

Die Optionsauswertung endet beim ersten Operanden: `yes a -x` schreibt
`a -x`. Eine unbekannte Option vor den Operanden ist ein Fehler; mit
`yes -- -x` lässt sich eine Zeichenkette ausgeben, die wie eine Option
aussieht.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.
- `--` — die Optionsauswertung beenden; jedes spätere Argument ist ein
  Operand.

## EXAMPLES

- `yes` — `y` bis zur Unterbrechung ausgeben.
- `yes hello world` — `hello world` bis zur Unterbrechung ausgeben.
- `yes -- -x` — `-x` ausgeben (nach `--` dürfen Operanden wie Optionen
  aussehen).

## EXIT STATUS

- `0` — die angeforderte Kurzhilfe wurde geliefert.
- `1` — die Ausgabe nimmt keine Bytes mehr an (die einzige
  Abbruchbedingung des Werkzeugs).
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `true`
- `man`
