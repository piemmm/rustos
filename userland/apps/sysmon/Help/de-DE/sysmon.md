## NAME

sysmon — Speicher und Last des Kernels live beobachten

## SYNOPSIS

`sysmon [-d Sek.Zehntel] [-h | -?]`

## DESCRIPTION

Zeigt eine live aktualisierte Vollbildansicht von Speicher und Last des
Kernels über die Systeminformations-API: physischer Speicher, der
Kernel-Heap, das Speicherdruck-Band samt Verlauf, das Verzeichnis der
zurückforderbaren Caches, die komprimierte `ramzip`-Stufe, die Summe
des angehefteten Speichers, die Last je CPU und eine Prozesszählung.
Das Werkzeug bleibt auch unter gezielter Belastung benutzbar und ruht
im Leerlauf zwischen den Auffrischungen.

Beim Start heftet der Monitor seinen eigenen Speicher an (`mem_pin`,
erfordert `CAP_MEM_PIN`), damit er unter dem beobachteten Druck nie an
seinem eigenen Seiteneinlagern hängen bleibt. Eine verweigerte
Anheftung wird in der Titelzeile gemeldet, und die Sitzung läuft ohne
Anheftung weiter — sie ist beiläufig, nie fatal.

Die Anzeige frischt sich in jedem Intervall selbst auf (3,0 Sekunden,
sofern `-d` nichts anderes bestimmt), und `r` frischt sie sofort auf.
Der Monitor nimmt keine Operanden an: er wird mit Tasten innerhalb der
Sitzung gesteuert.

- `q` — beenden.
- `p` — das Detailfeld weiterschalten: zurückforderbare Caches, die
  komprimierte Stufe, Last je CPU, Prozesse.
- `r` — sofort auffrischen.
- `+` / `-` — das Intervall um eine Sekunde verlängern / verkürzen,
  zwischen 0,1 und 60 Sekunden.
- Auf/Ab, BildAuf/BildAb, Pos1/Ende — das Detailfeld rollen.
- `h`, `?` — die Tastenübersicht ein- und ausblenden.

Sechs Übersichtszeilen stehen über dem Detailfeld: der Titel
(Betriebszeit, Lastmittel und Anheftungszustand); die Speicherwerte in
MiB samt angehefteter Summe; das Druckband mit Tiefenanzeige, Frei- und
Reservewerten und Eintrittszählern; der Bandverlauf (ein Zeichen je
Auffrischung: `.` normal, `-` mild, `=` moderat, `#` schwer, `!`
kritisch); die CPU-Gesamtzeile; und die Prozesszählung.

Jeder Wert läuft über die Systeminformations-API — es gibt kein
`/proc`. Die kernelweiten Statistikabfragen erfordern
`CAP_SYSINFO_KERNEL`, die systemweite Prozesszählung
`CAP_SYSINFO_GLOBAL`: wem eines fehlt, dem wird die Ablehnung des
jeweiligen Feldes ausbuchstabiert, während der Rest der Sitzung
weiterläuft. Die vollständige interaktive Prozessliste ist Aufgabe von
`top`; das Prozessfeld zeigt hier nur die Zählung und die größten
Verbraucher nach `%CPU` und Speicher.

## OPTIONS

- `-d, --delay <seconds>` — das Intervall zwischen den automatischen
  Auffrischungen, in Sekunden mit optionalem Bruchteil (nur die erste
  Nachkommastelle, Zehntel, wird behalten): `sysmon -d 1.5` frischt
  alle 1,5 Sekunden auf. Vorgabe 3,0. GNU `top` erlaubt ein Intervall
  von null und frischt so schnell wie möglich auf; TAIRiX läuft nie in
  einer Auslastungsschleife, daher wird null auf das Minimum von 0,1 s
  angehoben.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen und beenden.
  Innerhalb einer laufenden Sitzung schalten dieselben Tasten
  stattdessen die Tastenübersicht um.

## EXIT STATUS

- `0` — die Sitzung endete mit `q`, oder die Kurzhilfe wurde gezeigt.
- `1` — das Terminal versagte; der Grund steht auf der
  Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache der Kurzhilfe (ein BCP-47-Kürzel wie
  `de-DE`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
