## NAME

reset — das Terminal in einen ordentlichen Zustand zurückversetzen

## SYNOPSIS

`reset`

## DESCRIPTION

Macht den Zustand rückgängig, den ein abgestürztes Vollbildprogramm
hinterlassen kann. Zuerst wird die Eingabedisziplin auf den
interaktiven Standard zurückgesetzt (getippte Zeichen erscheinen
wieder). Danach wird die Wiederherstellungssequenz geschrieben: den
alternativen Bildschirm verlassen, den Cursor wieder anzeigen, Farben
und Attribute zurücksetzen, die Scrollregion zurücksetzen und
schließlich den Cursor in die linke obere Ecke setzen und die Anzeige
löschen.

Welche Operationen geschrieben werden, bestimmt das in `TERM` benannte
Terminal; eine Operation, die das Terminal nicht versteht, wird
weggelassen. Ein Terminal ganz ohne Steuerung (ein unbekanntes `TERM`
fällt auf das Minimalprofil zurück) erhält nur die Wiederherstellung
der Eingabedisziplin.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `reset` — das Terminal nach dem Absturz eines Vollbildprogramms
  wiederherstellen.

## EXIT STATUS

- `0` — das Terminal wurde wiederhergestellt.
- `1` — die Ausgabe konnte nicht zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `TERM` — das Terminal, dessen Wiederherstellungssequenz geschrieben
  wird.
- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `clear`
- `man`
