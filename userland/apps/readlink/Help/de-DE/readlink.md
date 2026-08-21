## NAME

readlink — das Ziel einer symbolischen Verknüpfung ausgeben

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] Datei...`

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

Die GNU-Kanonisierungsoptionen `-f`, `-e` und `-m` werden **abgewiesen**,
nicht angenähert. Jede Komponente eines Pfades aufzulösen — jeder
Verknüpfung folgen, `..` physisch behandeln, das Sprungbudget und die
Regel durchsetzen, dass eine Verknüpfung den speichernden Datenträger
nicht verlassen kann — ist die eine Implementierung des Dateisystems.
Eine zweite Kopie hier könnte einen Pfad ausgeben, den das Dateisystem
anders auflöst; die Option scheitert daher, bis das Dateisystem diese
Auflösung selbst anbietet.

## OPTIONS

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
- `readlink -z a b | tr '\0' '\n'` — NUL-getrennte Ziele für ein
  Skript.

## EXIT STATUS

- `0` — jedes Ziel wurde ausgegeben (oder die Kurzhilfe geschrieben).
- `1` — mindestens eine Leseoperation wurde abgewiesen, oder die Ausgabe
  ist fehlgeschlagen.
- `2` — die Befehlszeile wurde nicht verstanden oder nannte eine
  Kanonisierungsoption.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein
  BCP-47-Kennzeichen wie `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
