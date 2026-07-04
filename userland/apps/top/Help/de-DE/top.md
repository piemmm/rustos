## NAME

top — die Prozessliste live beobachten

## SYNOPSIS

`top [-h | -?]`

## DESCRIPTION

Zeigt eine live aktualisierte Vollbildansicht der Prozessliste über die
Systeminformations-API, im Geiste des klassischen `top`. Es startet mit
den Prozessen des Aufrufers; die systemweite Sicht gewährt der Dienst
nur einem Aufrufer mit `CAP_SYSINFO_GLOBAL`.

Der Betrachter nimmt keine Operanden an: er wird mit Tasten innerhalb
der Sitzung gesteuert.

- `q` — beenden.
- `a` — zwischen den eigenen Prozessen und der systemweiten Sicht
  umschalten.
- `r` — die Liste auffrischen.
- Hoch/Runter, BildAuf/BildAb, Pos1/Ende — die Auswahl bewegen.
- `h`, `?` — die Tastenübersicht ein- oder ausblenden.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen und beenden. In
  einer laufenden Sitzung schalten dieselben Tasten stattdessen die
  Tastenübersicht um.

## EXIT STATUS

- `0` — die Sitzung endete mit `q`, oder die Kurzhilfe wurde angezeigt.
- `1` — der Dienst oder das Terminal versagte.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
