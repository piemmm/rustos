## NAME

dirname — die letzte Komponente aus Namen entfernen

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Gibt jede Pfadschreibweise ohne ihre letzte Komponente aus:
abschließende Schrägstriche werden entfernt, dann die letzte Komponente
und die Schrägstriche davor. Der Eingriff ist rein lexikalisch — kein
Pfad wird aufgelöst oder auf der Platte berührt. Eine Schreibweise ohne
verbleibenden Schrägstrich hat den Elternteil `.`; ein Elternteil, der
sich leert, ist die Wurzel.

In eine Wurzel wird nie hineingeschnitten: `dirname /tools` ist `/`,
und — das Gegenstück im TAIRiX-Speicherwald — `dirname Home:/tools` ist
`Home:/`. Eine Alias-Wurzel (`Home:/`, `System:/`, …) spielt genau die
Rolle, die `/` auf POSIX-Systemen spielt.

## OPTIONS

- `-z, --zero` — jedes Ergebnis mit NUL statt Zeilenumbruch beenden.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `dirname /System/Commands/top.app` — `/System/Commands` ausgeben.
- `dirname src/lib.rs` — `src` ausgeben.
- `dirname file` — `.` ausgeben (kein Verzeichnisteil).
- `dirname Home:/tools` — `Home:/` ausgeben (in eine Wurzel wird nie
  hineingeschnitten).

## EXIT STATUS

- `0` — die Ergebnisse (oder die Kurzhilfe) wurden geschrieben.
- `1` — die Ausgabe konnte nicht zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `basename`
- `man`
