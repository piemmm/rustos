## NAME

true — nichts tun, mit Erfolg

## SYNOPSIS

`true [ignorierte Argumente]`

## DESCRIPTION

Beendet sich mit dem Status `0` und ignoriert dabei jedes Argument.
Skripte verwenden es überall dort, wo ein Befehl gebraucht wird, der
immer gelingt — als Platzhalterbefehl, als stets wahre Bedingung oder
als Rumpf einer Schleife.

Nur ein **erstes** Argument `-h`, `-?` oder `--help` wird beachtet (die
Position, in der GNU `true` `--help` beachtet); an jeder späteren
Position werden diese Wörter wie alles andere ignoriert.

## OPTIONS

- `-h, -?` — (nur als erstes Argument) die Kurzhilfe dieses Befehls
  anzeigen.

## EXAMPLES

- `true` — erfolgreich beenden.
- `while true; do …; done` — bis zur Unterbrechung wiederholen.

## EXIT STATUS

- `0` — immer (der ganze Zweck des Werkzeugs).
- `1` — eine angeforderte Kurzhilfe konnte nicht geschrieben werden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `false`
- `man`
