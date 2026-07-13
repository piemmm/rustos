## NAME

configure — die Systemkonfiguration zum Startzeitpunkt lesen und setzen

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Listet, zeigt und setzt die Einstellungen des Konfigurationsspeichers
unter `/System/Settings/Configuration/system.conf`. Ohne Operand wird
jede Einstellung mit ihrem aktuellen Wert aufgelistet; mit einem
Schlüssel allein wird dessen Wert angezeigt; mit Schlüssel und Wert
wird die Einstellung geändert.

Der Speicher liegt auf dem verschlüsselten Root-Datenträger und wird
von seinen Verbrauchern nach dem Entsperren des Root-Dateisystems
gelesen; eine Änderung wirkt daher beim nächsten Start ihres
Verbrauchers (`os.loginType`: die Anmeldung des nächsten Systemstarts).

Die Schlüsselmenge ist geschlossen: ein unbekannter Schlüssel oder ein
Wert außerhalb der Menge eines Schlüssels wird mit Angabe der gültigen
Auswahl abgelehnt und ändert nichts. Das Ändern einer Einstellung
schreibt den Speicher in kanonischer Form neu und erfordert
Schreibzugriff auf `/System/Settings` — ein gewöhnliches Konto kann die
Einstellungen lesen, aber nicht ändern.

- `os.loginType` — `text` oder `graphical`: welchen Sitzungstyp der
  Anmeldedienst für einen authentifizierten Benutzer startet. `text`
  (die Vorgabe) startet die Shell des Kontos — der Desktop lässt sich
  weiterhin bei Bedarf mit dem Befehl `desktop` starten; `graphical`
  startet nach der Authentifizierung direkt die Desktop-Sitzung, sofern
  ein Desktop installiert ist, und fällt andernfalls auf Text zurück.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `configure` — jede Einstellung auflisten.
- `configure os.loginType` — den vorgegebenen Sitzungstyp anzeigen.
- `configure os.loginType graphical` — in die grafische Anmeldung
  starten.

## EXIT STATUS

- `0` — Auflistung, Wert, Kurzhilfe oder Änderung wurden abgeschlossen.
- `1` — der Speicher konnte nicht gelesen oder geschrieben werden
  (z. B. darf der Aufrufer Systemeinstellungen nicht ändern) oder die
  Ausgabe konnte nicht zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden, der Schlüssel ist
  unbekannt oder der Wert liegt außerhalb der Menge des Schlüssels.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache der Kurzhilfe (ein BCP-47-Kennzeichen
  wie `fr-FR`).

## SEE ALSO

- `man`
