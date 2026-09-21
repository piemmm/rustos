## NAME

groupdel — eine Gruppe löschen

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Entfernt eine Gruppe aus dem Gruppenverzeichnis. Das Löschen einer Gruppe ist ein Verwaltungsvorgang: Das Verzeichnis weist einen Aufrufer ohne die Benutzerverwaltungsfähigkeit ab.

Das Verzeichnis entscheidet, was entfernt werden darf. Es verweigert das Löschen einer Gruppe, auf die ein Konto noch verweist, damit kein Konto eine nicht vorhandene Gruppe benennt.

`--` beendet die Optionsauswertung: Jedes spätere Argument ist ein Operand.

## OPTIONS

- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `groupdel staff` — die Gruppe `staff` löschen.

## EXIT STATUS

- `0` — die Gruppe wurde gelöscht.
- `1` — das Verzeichnis hat das Löschen verweigert oder es ist fehlgeschlagen (etwa eine fehlende Fähigkeit, eine unbekannte Gruppe oder eine noch referenzierte Gruppe); der Grund erscheint auf der Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel wie `de-DE`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
