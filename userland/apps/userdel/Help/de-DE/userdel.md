## NAME

userdel — ein Benutzerkonto löschen

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Entfernt ein Konto aus der Benutzerdatenbank. Das Löschen eines Kontos ist ein Verwaltungsvorgang: Die Datenbank weist einen Aufrufer ohne die Benutzerverwaltungsfähigkeit ab.

Die Datenbank entscheidet, was entfernt werden darf. Sie verweigert das Löschen des letzten aktiven Kontos, das Benutzer verwalten darf, damit ein System nie ohne Verwaltungsmöglichkeit zurückbleibt.

`--` beendet die Optionsauswertung: Jedes spätere Argument ist ein Operand.

## OPTIONS

- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `userdel ada` — das Konto `ada` löschen.

## EXIT STATUS

- `0` — das Konto wurde gelöscht.
- `1` — die Datenbank hat das Löschen verweigert oder es ist fehlgeschlagen (etwa eine fehlende Fähigkeit, ein unbekanntes Konto oder der letzte Administrator); der Grund erscheint auf der Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel wie `de-DE`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
