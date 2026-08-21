## NAME

unlink — einen einzelnen Namen entfernen

## SYNOPSIS

`unlink [--] Datei`

## DESCRIPTION

Entfernt genau einen Namen, über den einen Dateisystemaufruf, den die
POSIX-Funktion `unlink` benennt. Es gibt absichtlich keine Rekursion,
kein Erzwingen, keine Rückfrage und keine Meldungen: ein Skript, das
genau einen Namen und nichts weiter entfernen muss, hat damit ein
Werkzeug, das nicht mehr kann. Für jene Optionen dient `rm`, für ein
Verzeichnis `rmdir`.

Der Name wird **wie geschrieben** entfernt. Eine symbolische
Verknüpfung wird selbst entfernt und nie verfolgt, sodass eine dort
platzierte Verknüpfung die Entfernung nicht auf ihr Ziel umlenken kann.

Ein **Verzeichnis** weist das Dateisystem ab, im selben gesperrten
Durchlauf, der den Eintrag entfernt hätte — ein Wettlauf zwischen Prüfen
und Entfernen existiert hier nicht.

Genau ein Operand ist erforderlich: kein Operand und zwei oder mehr
Operanden sind beides Benutzungsfehler, und nichts wird entfernt. `--`
beendet die Optionsauswertung, sodass ein Name mit führendem
Bindestrich entfernbar bleibt.

## OPTIONS

- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `unlink alt.log` — einen Namen entfernen.
- `unlink Home:/Documents/alias` — die symbolische Verknüpfung selbst
  entfernen, nicht ihr Ziel.
- `unlink -- -seltsamer-name` — einen Namen mit führendem Bindestrich
  entfernen.

## EXIT STATUS

- `0` — der Name wurde entfernt (oder die Kurzhilfe wurde geschrieben).
- `1` — das Dateisystem hat die Entfernung abgelehnt, oder die Ausgabe
  ist fehlgeschlagen; der Grund steht auf der Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kennzeichen
  wie `fr-FR`).

## SEE ALSO

rm, rmdir, ln, link, readlink
