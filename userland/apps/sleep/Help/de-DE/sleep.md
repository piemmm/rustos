## NAME

sleep — für die Summe von Zeitintervallen pausieren

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Pausiert für die Summe der angegebenen Intervalle und beendet sich dann.

Jede `NUMBER` ist ein Gleitkommawert; ein einbuchstabiges `SUFFIX`
skaliert ihn: `s` für Sekunden (Vorgabe), `m` für Minuten, `h` für Stunden
und `d` für Tage. Mehrere Operanden werden addiert, sodass `sleep 1m 30s`
neunzig Sekunden pausiert. `inf` (oder `infinity`) pausiert, bis der
Prozess beendet wird.

Anders als die eigene Zeitmessung einer Shell schläft `sleep` außerhalb des
Prozessors: die Aufgabe wird geparkt, bis das Intervall abgelaufen ist, und
lässt niemals einen Kern leerlaufen.

Ein negativer Wert, ein `nan`, ein unbekanntes Suffix oder zusätzliche
Zeichen nach der Zahl sind ein `invalid time interval`. Gar kein Operand
ist ein `missing operand`.

Dieser Befehl gibt keine Systemversion aus; TAIRiX hat keine solche
Zeichenkette, daher hat er — anders als GNU `sleep` — keine Option
`--version`.

## OPTIONS

- `-h, -?` — die eigene Kurzhilfe dieses Befehls anzeigen.
- `--` — die Optionsauswertung beenden; jedes spätere Argument ist ein
  Operand.

## EXAMPLES

- `sleep 5` — fünf Sekunden pausieren.
- `sleep 1.5h` — neunzig Minuten pausieren.
- `sleep 1m 30s` — neunzig Sekunden pausieren (die Operanden werden
  addiert).
- `sleep inf` — pausieren, bis der Prozess beendet wird.

## EXIT STATUS

- `0` — das Intervall ist abgelaufen, oder eine angeforderte Kurzhilfe
  wurde geschrieben.
- `1` — das Schreiben der Kurzhilfe ist fehlgeschlagen.
- `2` — die Befehlszeile wurde nicht verstanden (unbekannte Option,
  fehlender Operand oder ungültiges Zeitintervall).

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `fr-FR`).

## SEE ALSO

- `top`
