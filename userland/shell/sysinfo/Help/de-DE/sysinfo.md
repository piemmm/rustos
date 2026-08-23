## NAME

sysinfo — Systeminformationen abfragen

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Stellt eine typisierte Abfrage an die Systeminformations-API und gibt
die Antwort aus. TAIRiX hat kein `/proc` und kein `/sys`: dieser Befehl
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
- `cpuinfo` — der Prozessorbericht je CPU (eine Obermenge von
  `/proc/cpuinfo`): Modell/Hersteller, Leistungsklasse, ISA-Erweiterungs-
  Flags, das rohe Identitätsregister, die live gemessene Kerntaktrate (in
  MHz — oder ein ehrliches „unknown“, wo es keinen Kerntaktzähler gibt)
  und die feste Referenz-/Zeitbasisfrequenz. Öffentliche
  Hardware-Fakten, keine Capability erforderlich.
- `irq`, `irqs` — die IRQ-Tabelle des Kernels: eine Zeile je gebundener
  Interrupt-Leitung — ihre Kennung, die besitzende Treiber-Task, die
  Anzahl der Interrupts seit dem Start und ob die Leitung unter
  Quarantäne steht (benötigt `CAP_SYSINFO_HW`).
- `storage`, `io` — die Speicher-E/A-Gesundheit je Datenträger: eine
  Zeile je fehlerbewusstem blockgestütztem Datenträger — ein Präfix
  seiner dauerhaften Kennung, der bedienende Blockdienst-Endpunkt, seine
  aktuelle Verfügbarkeit (available/degraded/recovering/lost) und die
  kumulativen Ergebniszähler (Abschlüsse, Resets, Zeitüberschreitungen,
  Medienfehler, Wiederholungen), an denen eine ausfallende oder
  flatternde Platte sichtbar wird (benötigt `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — die zusammengesetzten RAID-Verbünde und die Geräte,
  die der Verbund-Komponist hält: eine Zeile je Verbund — ein Präfix
  seiner Identität, sein Level, seine Gesundheit
  (optimal/degraded/recovering/failed), die Anzahl synchroner und
  definierter Mitglieder, seine Stripe-Einheit, seine Blockzahl und ein
  laufender Wiederaufbau oder Prüflauf — dann eine Zeile je Gerät —
  sein Hardwarebaum-Knoten, der Verbund, zu dem es gehört (ein
  Bindestrich für einen ungebundenen Kandidaten), sein Steckplatz, seine
  Rolle (candidate/held/in-sync/resyncing/faulted), seine Größe und die
  Metadaten-Generation, die es trägt (benötigt `CAP_SYSINFO_HW`).
- `show <resource-ref>` — liest eine `info:`/`state:`/`stats:`-Ressourcen­referenz
  und gibt ihren Wert aus. Diese Namensräume liefern typisierte Werte über
  diese API, niemals Byteströme — `cat` kann sie nicht öffnen. Eine
  Ablehnung nennt die benötigte Capability.
- `describe <resource-ref>` — gibt statt des Werts die Antwort-Hülle aus:
  Produzent, Autorisierung und die Metadaten der Nutzlast — bei einer
  Metrik Art, Einheit, Rücksetzverhalten und Messfenster; bei einer
  Tatsache Typ und Vertraulichkeit.
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
