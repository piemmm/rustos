## NAME

sysinfo — Systeminformationen abfragen

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Stellt eine typisierte Abfrage an die Systeminformations-API und gibt
die Antwort aus. RustOS hat kein `/proc` und kein `/sys`: dieser Befehl
ist das Terminal-Gesicht derselben versionierten, durch Capabilities
geprüften API, die jedes Programm benutzt, und kein Pfad umgeht die
Capability-Prüfung.

Die Abfragen:

- `processes`, `ps` — Prozesse auflisten, eine Zeile pro Prozess.
- `memory`, `mem` — Kernel-Speicherstatistiken (benötigt
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — der erkannte Hardwarebaum (benötigt
  `CAP_SYSINFO_HW`).
- `identity`, `id` — Maschinenidentität und OS-Version.
- `uptime` — Zeit seit dem Start und die Startzeit als Wanduhrzeit.
- `limits`, `rlimits` — die eigenen wirksamen Ressourcenlimits und ihre
  aktuelle Nutzung.
- `seats` — das Sitzinventar: der Besitzer jedes Displays und seine
  Vordergrundkonsole (benötigt `CAP_SYSINFO_HW`).
- `pressure` — die Live-Speicherdruckanzeige: Band, Wasserstände und
  Übergangszähler (benötigt `CAP_SYSINFO_KERNEL`).
- `reclaim` — das Register der rückgewinnbaren Caches, eine Zeile pro
  Klasse (benötigt `CAP_SYSINFO_KERNEL`).
- `ramzip` — die Zähler der komprimierten Speicherstufe (benötigt
  `CAP_SYSINFO_KERNEL`).
- `cpu` — Warteschlangentiefe, Kontextwechsel und Präemptionen je CPU
  (benötigt `CAP_SYSINFO_KERNEL`).
- `help` — die Kurzhilfe dieses Befehls.

Ohne Abfrage wird die Kurzhilfe angezeigt.

## OPTIONS

- `--all, -a` — mit `processes`: alle Prozesse des Systems auflisten
  statt nur die eigenen; der Dienst gewährt diese Sicht nur einem
  Aufrufer mit `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `sysinfo identity` — die Maschinenidentität und OS-Version ausgeben.
- `sysinfo ps --all` — alle Prozesse des Systems auflisten.

## EXIT STATUS

- `0` — die Abfrage wurde beantwortet und ausgegeben.
- `1` — der Dienst hat abgelehnt oder versagt, oder das Ergebnis konnte
  nicht ausgegeben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `man`
- `ps`
- `top`
