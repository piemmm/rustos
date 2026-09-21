## NAME

passwd — das Kennwort eines Kontos setzen

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Ersetzt das gespeicherte Kennwort des genannten Kontos. Ein Kennwort zu
setzen ist ein Verwaltungsvorgang: Die Datenbank weist einen Aufrufer ohne
die Benutzerverwaltungsfähigkeit ab.

Ein Klartextkennwort überquert den Systemaufruf nie. Das Werkzeug fragt
zweimal mit abgeschaltetem Terminalecho, bildet aus dem Getippten einen
gesalzenen PBKDF2-Datensatz — das Salz stammt aus der Zufallsquelle des
Kerns — und sendet den Datensatz; beide Klartextpuffer werden genullt,
sobald er vorliegt.

Der Kontoname ist erforderlich. GNUs `passwd` ohne Operand ändert das eigene
Kennwort des Aufrufers, wofür TAIRiX einen unprivilegierten
Selbstbedienungsweg bräuchte, den es nicht gibt: Die gesamte
Kontoverwaltungsschnittstelle ist fähigkeitsgeschützt, und darin „den
eigenen Datensatz“ auszunehmen wäre eine Änderung des Sicherheitsmodells,
keine Bequemlichkeit.

Ein Aufrufer ohne Terminal — ein grafisches Programm, dessen Standardeingabe
unter dem Erhöhungsvermittler geschlossen ist — bildet den Datensatz selbst
und übergibt ihn mit `--record`, sodass auf keiner Seite Klartext entsteht.
Der Datensatz wird vor dem Speichern auf Wohlgeformtheit geprüft.

`--` beendet die Optionsauswertung: Jedes spätere Argument ist ein Operand.

## OPTIONS

- `--record RECORD` — ein fertiger gesalzener PBKDF2-Datensatz, für einen
  Aufrufer ohne Terminal zum Nachfragen.
- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `passwd ada` — zweimal fragen und das Kennwort des Kontos setzen.

## EXIT STATUS

- `0` — das Kennwort wurde ersetzt.
- `1` — die Datenbank hat das Ersetzen verweigert oder es ist
  fehlgeschlagen, die Eingaben stimmten nicht überein, es war kein Zufall
  verfügbar, oder der Datensatz war fehlerhaft; der Grund erscheint auf der
  Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel wie `de-DE`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
