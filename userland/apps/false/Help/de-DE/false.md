## NAME

false — nichts tun, ohne Erfolg

## SYNOPSIS

`false [ignorierte Argumente]`

## DESCRIPTION

Beendet sich mit dem Status `1` und ignoriert dabei jedes Argument.
Skripte verwenden es überall dort, wo ein Befehl gebraucht wird, der
immer fehlschlägt — als stets falsche Bedingung oder als absichtlicher
Fehlschlag.

Nur ein **erstes** Argument `-h`, `-?` oder `--help` wird beachtet (die
Position, in der GNU `false` `--help` beachtet); an jeder späteren
Position werden diese Wörter wie alles andere ignoriert. Anders als GNU
`false --help`, das trotzdem mit `1` endet, endet eine gelieferte
Kurzhilfe hier mit `0` — die RustOS-Kurzhilfe-Konvention.

## OPTIONS

- `-h, -?` — (nur als erstes Argument) die Kurzhilfe dieses Befehls
  anzeigen.

## EXAMPLES

- `false` — fehlschlagen.
- `until false; do …; done` — den Rumpf einmal ausführen (die
  Bedingung ist immer falsch).

## EXIT STATUS

- `1` — immer (der ganze Zweck des Werkzeugs).
- `0` — die angeforderte Kurzhilfe wurde geliefert.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `true`
- `man`
