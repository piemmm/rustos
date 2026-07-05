## NAME

groupadd — eine Gruppe anlegen

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Fügt dem Gruppenregister eine einzelne Gruppe hinzu. Der Gruppenname
muss `[a-z_][a-z0-9_-]*` entsprechen, und die Kennung ist ein dezimaler
Wert. Das Anlegen einer Gruppe ist ein Verwaltungsvorgang: das Register
weist einen Aufrufer ohne die Benutzerverwaltungs-Berechtigung ab.

Wird `-g` weggelassen, wird die Gruppenkennung automatisch vergeben,
eins über der höchsten vorhandenen Kennung. Eine angeforderte, bereits
vergebene Kennung wird abgewiesen; das Register ist die Autorität über
Kollisionen.

`--` beendet die Optionsauswertung: jedes spätere Argument ist ein
Operand.

## OPTIONS

- `-g, --gid GID` — numerische Gruppenkennung; wird bei Weglassen
  automatisch vergeben (eins über der höchsten vorhandenen).
- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `groupadd staff` — `staff` mit automatisch vergebener Kennung anlegen.
- `groupadd -g 100 staff` — `staff` mit der Kennung `100` anlegen.

## EXIT STATUS

- `0` — die Gruppe wurde angelegt.
- `1` — das Register hat die Anlage abgelehnt oder sie ist
  fehlgeschlagen (etwa eine fehlende Berechtigung oder eine doppelte
  Kennung); der Grund wird auf der Standardfehlerausgabe ausgegeben.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `useradd`
- `users`
