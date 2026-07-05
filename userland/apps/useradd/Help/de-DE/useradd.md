## NAME

useradd — ein Benutzerkonto anlegen

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Fügt der Benutzerdatenbank ein einzelnes Konto hinzu. Der Anmeldename
muss `[a-z_][a-z0-9_-]*` entsprechen; die primäre Gruppe (`-g`) ist
erforderlich, und jede Gruppen- oder Benutzerreferenz ist eine dezimale
Kennung. Das Anlegen eines Kontos ist ein Verwaltungsvorgang: die
Datenbank weist einen Aufrufer ohne die
Benutzerverwaltungs-Berechtigung ab.

Das angelegte Konto hat **kein nutzbares Passwort**: kein Passwort passt
darauf, bis ein Administrator eines setzt (und keines kann erraten
werden) — genau wie das GNU-Werkzeug ein deaktiviertes Konto anlegt.
Setzen Sie anschließend ein Passwort mit dem Befehl `passwd` des
Werkzeugs `users`.

Wird `-u` weggelassen, wird die Benutzerkennung automatisch vergeben,
eins über der höchsten vorhandenen Kennung. Wird `-d` weggelassen, folgt
das Heimatverzeichnis dem Standardlayout `/Users/NAME`. Das Konto
startet die Standard-Shell des Systems und die gewöhnliche
Sitzungs-Berechtigungsobergrenze; ein Administrator erweitert sie
anschließend mit dem Befehl `grant` des Werkzeugs `users`.

`--` beendet die Optionsauswertung: jedes spätere Argument ist ein
Operand.

## OPTIONS

- `-u, --uid UID` — numerische Benutzerkennung; wird bei Weglassen
  automatisch vergeben (eins über der höchsten vorhandenen).
- `-g, --gid GID` — numerische Kennung der primären Gruppe.
  Erforderlich: es gibt keine zu erratende Standardgruppen-Politik.
- `-G, --groups LIST` — kommagetrennte numerische Kennungen der
  zusätzlichen Gruppen.
- `-c, --comment TEXT` — Kontokommentar / vollständiger Anzeigename.
- `-d, --home PATH` — Heimatverzeichnis; `/Users/NAME` bei Weglassen.
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `useradd -g 100 alice` — `alice` in der primären Gruppe `100` mit
  automatisch vergebener Kennung anlegen.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — jedes Feld
  ausgeschrieben.

## EXIT STATUS

- `0` — das Konto wurde angelegt.
- `1` — die Datenbank hat die Anlage abgelehnt oder sie ist
  fehlgeschlagen (etwa eine fehlende Berechtigung, eine doppelte Kennung
  oder eine unbekannte Gruppe); der Grund wird auf der
  Standardfehlerausgabe ausgegeben.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `groupadd`
- `users`
