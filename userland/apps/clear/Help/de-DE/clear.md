## NAME

clear — den Terminalbildschirm löschen

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Schreibt die Sequenz, die den Cursor in die linke obere Ecke setzt und
die gesamte Anzeige löscht, sodass ein leerer Bildschirm bleibt. Welche
Sequenz geschrieben wird, bestimmt das in `TERM` benannte Terminal; ein
Terminal, das nicht löschen kann (ein unbekanntes `TERM` fällt auf das
Minimalprofil zurück), lässt den Befehl fehlschlagen, statt Bytes zu
drucken, die das Terminal als Zeichenmüll darstellen würde.

TAIRiX-Konsolen führen keinen Verlaufspuffer, es gibt also nichts
zurückzuscrollen: `-x` (die GNU-Option, die den Verlauf erhält) wird
aus Skript-Kompatibilität akzeptiert und ändert nichts.

## OPTIONS

- `-x` — aus GNU-Kompatibilität akzeptiert; eine TAIRiX-Konsole führt
  keinen Verlauf, die Ausgabe ist mit und ohne identisch.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `clear` — den Bildschirm löschen.

## EXIT STATUS

- `0` — die Löschsequenz wurde geschrieben.
- `1` — das Terminal kann nicht löschen, oder die Ausgabe konnte nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `TERM` — das Terminal, dessen Löschsequenz geschrieben wird.
- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `reset`
- `man`
