## NAME

readlink — das Ziel einer symbolischen Verknüpfung ausgeben

## SYNOPSIS

`readlink [-fem] [-nz] [-q | -s | -v] [--] Datei...`

## DESCRIPTION

Gibt das Ziel aus, das jeder Operand speichert — eines je Operand, in der
Reihenfolge der Befehlszeile.

Das Ziel wird **wie gespeichert** ausgegeben. Das Ziel einer Verknüpfung
ist ein Datum, kein bei ihrer Erzeugung aufgelöster Pfad: es darf relativ
sein, `..` enthalten und überhaupt nichts benennen. `readlink` zeigt also
die Schreibweise, und `ls -l` zeigt eine Verknüpfung neben dem, was sie
gerade benennt.

Ein Operand, der **keine** symbolische Verknüpfung ist, hat kein Ziel
auszugeben — eine Datei und ein Verzeichnis werden beide mit demselben
Grund „Wert außerhalb des Bereichs" abgewiesen — und ein fehlender Name
ist „nicht gefunden". In beiden Fällen werden die übrigen Operanden noch
gelesen, und der Befehl endet mit einem Status ungleich null. Still ist
die Vorgabe wie im GNU-Werkzeug: `-v` schaltet die Meldungen je Operand
ein.

`-n` lässt den Trenner nach dem letzten Ziel weg. Bei mehr als einem
Operanden wird es ignoriert, und das wird gemeldet, denn die Trenner
zwischen den Zielen sind es, die sie trennen.

Mindestens ein Operand ist erforderlich. `--` beendet die
Optionsauswertung.

`-f`, `-e` und `-m` schalten stattdessen auf **Kanonisierung** um: den
einen Pfad, der benennt, worauf der Operand auflöst, mit jeder
Verknüpfung verfolgt und jedem `..` angewandt. Unter keiner von ihnen
muss der Operand überhaupt eine Verknüpfung sein; die drei unterscheiden
sich nur darin, wie viel des Pfades vorhanden sein muss. Sie sind
Alternativen und keine Zusätze, also gewinnt die letzte genannte.

Diese Auflösung gehört dem Dateisystem — physisches `..`, das
Sprungbudget, eine Suchrechtsprüfung für jedes durchlaufene Verzeichnis
und die Regel, dass eine Verknüpfung nicht außerhalb dessen auflösen
kann, was ihre Einhängung projiziert — und dieses Werkzeug *ruft* sie
auf, statt selbst Verknüpfungen zu verfolgen. Eine zweite Kopie des
Algorithmus, die in einer Regel abwiche, gäbe einen Pfad aus, den das
Dateisystem anders auflöst.

## OPTIONS

- `-f, --canonicalize` — den kanonischen Pfad ausgeben; jede Komponente
  außer der letzten muss vorhanden sein.
- `-e, --canonicalize-existing` — den kanonischen Pfad ausgeben; jede
  Komponente muss vorhanden sein.
- `-m, --canonicalize-missing` — den kanonischen Pfad ausgeben; keine
  Komponente muss vorhanden sein.
- `-n, --no-newline` — den Trenner nach dem letzten Ziel nicht ausgeben
  (bei mehr als einem Operanden ignoriert, mit Meldung).
- `-z, --zero` — jedes Ziel mit NUL statt Zeilenumbruch beenden.
- `-q, -s` — eine abgewiesene Leseoperation nicht melden (die Vorgabe;
  auch `--quiet`, `--silent`).
- `-v, --verbose` — eine abgewiesene Leseoperation auf der
  Standardfehlerausgabe melden.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — ausgeben, was eine Verknüpfung
  speichert.
- `readlink -v alias` — ausgeben und sagen, warum nicht, falls es keine
  Verknüpfung ist.
- `readlink -f alias` — ausgeben, worauf es auflöst, Verknüpfungen und
  alles.
- `readlink -z a b | tr '\0' '\n'` — NUL-getrennte Ziele für ein
  Skript.

## EXIT STATUS

- `0` — jedes Ziel wurde ausgegeben (oder die Kurzhilfe geschrieben).
- `1` — mindestens eine Leseoperation wurde abgewiesen, oder die Ausgabe
  ist fehlgeschlagen.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein
  BCP-47-Kennzeichen wie `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
