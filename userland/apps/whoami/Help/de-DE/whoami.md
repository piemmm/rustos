## NAME

whoami — den Kontonamen des aktuellen Benutzers ausgeben

## SYNOPSIS

`whoami`

## DESCRIPTION

Gibt den Benutzernamen aus, der zur Identität dieses Prozesses gehört,
gefolgt von einem Zeilenumbruch — und sonst nichts.

TAIRiX hat kein `/etc/passwd`: Die Benutzerkennung stammt aus dem
Eintrag, den der Kernel über den aufrufenden Prozess führt, und der
zugehörige Kontoname aus dem öffentlichen Kontoverzeichnis der
Systeminformations-API. Enthält das Verzeichnis keinen Namen für die
Kennung, meldet der Befehl `cannot find name for user ID <uid>` und
schlägt fehl.

Der Befehl nimmt keine Operanden an; ein Argument ist ein Fehler
`extra operand`.

## OPTIONS

- `-h, -?` — die kurze Hilfe dieses Befehls anzeigen.
- `--` — die Optionsauswertung beenden; jedes spätere Argument bleibt
  ein überzähliger Operand (`whoami` nimmt keine an).

## EXAMPLES

- `whoami` — den Namen des Kontos ausgeben, das den Befehl ausführt.

## EXIT STATUS

- `0` — der Name (oder die angeforderte kurze Hilfe) wurde geschrieben.
- `1` — das Lesen der Identität, die Verzeichnisabfrage oder die
  Ausgabe schlug fehl, oder das Verzeichnis enthält keinen Namen für
  die Benutzerkennung.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die kurze Hilfe (ein BCP-47-Tag
  wie `de-DE`).

## SEE ALSO

- `users`
- `ps`
