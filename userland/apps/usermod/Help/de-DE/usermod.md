## NAME

usermod — ein Benutzerkonto ändern

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Ändert die Identitätsfelder eines Kontos, seinen Sperrzustand oder seine
Fähigkeitsobergrenze. Das Ändern eines Kontos ist ein Verwaltungsvorgang:
Die Datenbank weist einen Aufrufer ohne die Benutzerverwaltungsfähigkeit ab.

Eine Identitätsänderung ersetzt den gesamten nicht sicherheitsrelevanten
Feldsatz, daher liest das Werkzeug zuerst den aktuellen Datensatz und sendet
jedes nicht benannte Feld unverändert zurück. Ein Konto, das die Datenbank
nicht führt, wird abgewiesen, bevor irgendetwas gesendet wird.

Jeder Schalter ist ein eigener Datenbankvorgang, einzeln und ganz oder gar
nicht angewendet. Eine Zeile, die mehrere verlangt, sendet mehrere in fester
Reihenfolge — Felder, dann Fähigkeiten, dann Sperrzustand — und hält beim
ersten Abweisen an, benennt den Schritt und warnt, dass eine frühere
Änderung bereits wirksam sein kann.

`-G` ersetzt den gesamten Zusatzsatz, statt anzuhängen: Die Datenbank nimmt
ganze Sätze, und ein Anhängen auf Grundlage einer veralteten Lesung wäre
schlechter als ein ausdrückliches Ersetzen. `--grants` ist ein
TAIRiX-Begriff ohne coreutils-Gegenstück und daher nur in Langform
geschrieben; die Datenbank weist jede Fähigkeit ab, die das aufrufende Konto
nicht selbst hält.

`--` beendet die Optionsauswertung: Jedes spätere Argument ist ein Operand.

## OPTIONS

- `-c, --comment COMMENT` — der Kontokommentar / vollständige Name.
- `-d, --home HOME` — das Heimatverzeichnis.
- `-s, --shell SHELL` — die Anmelde-Shell.
- `-g, --gid GID` — die numerische Kennung der Hauptgruppe.
- `-G, --groups LIST` — die durch Kommas getrennten numerischen
  Zusatzgruppen, die den aktuellen Satz ersetzen. Eine leere Liste löscht
  ihn.
- `-L, --lock` — dem Konto die Anmeldung verwehren.
- `-U, --unlock` — sie wieder erlauben.
- `--grants LIST` — die durch Kommas getrennten Fähigkeitsnamen, die die
  gesamte Obergrenze bilden. Eine leere Liste löscht sie.
- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — den vollständigen Namen setzen.
- `usermod -L ada` — das Konto sperren.
- `usermod --grants LIST` — die Obergrenze ersetzen.

## EXIT STATUS

- `0` — jede verlangte Änderung wurde vorgenommen.
- `1` — die Datenbank hat eine Änderung verweigert oder sie ist
  fehlgeschlagen; Schritt und Grund erscheinen auf der
  Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel wie `de-DE`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
