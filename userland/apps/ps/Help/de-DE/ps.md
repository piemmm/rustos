## NAME

ps — Prozesse auflisten

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Listet Prozesse über die Systeminformations-API auf. Standardmäßig
werden nur die Prozesse des Aufrufers aufgelistet; der Dienst wendet
jeden Abfragebereich anhand der vom Kernel bezeugten Identität des
Aufrufers an, und kein Pfad umgeht diese Prüfung.

Jeder Prozess wird als eine Zeile unter einer Spaltenüberschrift
ausgegeben: die Prozesskennung (`PID`), die Kennung des Elternprozesses
(`PPID`), die Benutzer- und Gruppenkennungen des Eigentümers (`UID`,
`GID`), der Scheduling-Zustand (`S`), die CPU, auf der der Prozess
zuletzt lief (`CPU`), und der Kommandoname (`NAME`).

`ps` nimmt keine Operanden an.

## OPTIONS

- `-e, -A, --all` — alle Prozesse des Systems auflisten statt nur die
  des Aufrufers; der Dienst gewährt diese Sicht nur einem Aufrufer mit
  `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `ps` — die eigenen Prozesse auflisten.
- `ps -e` — alle Prozesse des Systems auflisten.

## EXIT STATUS

- `0` — die Liste wurde geschrieben.
- `1` — der Dienst hat abgelehnt oder versagt, oder die Liste konnte
  nicht ausgegeben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
