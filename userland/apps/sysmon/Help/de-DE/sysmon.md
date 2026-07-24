## NAME

sysmon — Speicher, Caches und Kernel-Last live beobachten

## SYNOPSIS

`sysmon [-d Sek.Zehntel] [-h | -?]`

## DESCRIPTION

`sysmon` ist eine bildschirmfüllende Live-Ansicht dessen, was der Kernel
mit Speicher und CPU tut, vollständig über die Systeminformations-API
gelesen — es gibt kein `/proc` zum Auslesen. Es zeigt den physischen
Speicher und seine Zusammensetzung, den Kernel-Heap, das
Speicherdruck-Band und dessen jüngste Historie, das Register der
rückgewinnbaren Caches mit **Trefferquoten** je Klasse, die komprimierte
`ramzip`-Stufe, die Summe des angehefteten Speichers, die
Speichernutzung eingehängter Datenträger, die Last je CPU, die
Kernel-Interrupt-Tabelle und eine Prozesszählung. Es bleibt unter
absichtlicher Last benutzbar und ruht zwischen den Aktualisierungen im
Leerlauf (die Lesung parkt; sie dreht nie leer).

Beim Start heftet der Monitor seinen eigenen Speicher an (`mem_pin`, das
`CAP_MEM_PIN` erfordert), damit er unter dem Druck, den er beobachtet, nie
an seinen eigenen Seitenfehlern hängen bleibt. Ein abgelehntes Anheften
wird in der Titelzeile gemeldet und die Sitzung läuft ungeheftet weiter —
das Anheften ist beiläufig, nie fatal.

Die Anzeige aktualisiert sich in jedem Intervall (3,0 Sekunden, sofern
`-d` es nicht ändert). Der Monitor nimmt keine Operanden an: er wird durch
Tasten innerhalb der Sitzung gesteuert.

- `q` — beenden.
- Links / Rechts (oder `p`) — das Detailfeld wechseln (Links = vorheriges,
  Rechts / `p` = nächstes): Caches, die komprimierte Stufe, die
  Speichernutzung eingehängter Datenträger (Datenträger), die Last je CPU,
  die Interrupt-Leitungen, die Prozesse.
- `r` — jetzt aktualisieren.
- `+` / `-` — das Intervall um eine Sekunde verlängern / verkürzen,
  zwischen 0,1 und 60 Sekunden.
- Auf/Ab, Bild auf/Bild ab, Pos1/Ende — das fokussierte Feld rollen.
- `h`, `?` — die Tastenübersicht der Sitzung ein- oder ausblenden (sie gibt
  die Balken-Legende unten wieder).

### Der Zusammenfassungsblock

Ein fester Zusammenfassungsblock geht dem Detailfeld voran. Jede Zeile ist
links beschriftet, sodass sie ohne Farbe lesbar ist; Farbe ist nur
Verstärkung.

- **Titelzeile** — der Werkzeugname, die Systemlaufzeit (`up D days,
  H:MM`), die drei Lastmittel (1/5/15 Minuten) und der Anheft-Zustand
  (`[pinned]`, oder `[unpinned: <reason>]`, wenn das Anheften abgelehnt
  wurde).
- **`Mem`** — der Speicherbalken (siehe die Balken-Legende), gefolgt von
  belegten / gesamten MiB, dem belegten Prozentsatz, der Größe des
  Kernel-Heaps und — falls von null verschieden — den Werten des
  komprimierten `ramzip`-Speichers und des angehefteten `pinned`-Speichers.
- **`Pres`** — der Speicherdruck-Balken: eine Anzeige mit fünf Bändern,
  jedes erreichte Band in seiner eigenen Schweregradfarbe gefüllt, gefolgt
  vom Namen des aktuellen Bandes, den Werten frei / Reserve und der
  Gesamtzahl der Band-Eintritte.
- **`Hist`** — der Streifen der Druckband-Historie: ein Glyph je
  Aktualisierung, das älteste links, jedes nach seinem Band eingefärbt —
  `.` normal, `-` gering, `=` mäßig, `#` schwer, `!` kritisch — sodass eine
  Druckstrecke als farbige Folge lesbar ist.
- **`CPU`** — der aggregierte CPU-Balken (siehe die Balken-Legende),
  gefolgt vom Auslastungsprozentsatz aller CPUs, der CPU-Anzahl und den
  summierten Zählern für Kontextwechsel und Verdrängungen.
- **`Tasks`** — die Prozesszählung: gesamt, laufend, schlafend, gestoppt
  und Zombies (mit angehängtem `(own)`, wenn die Zählung aller Prozesse
  abgelehnt wurde und nur die eigenen Aufgaben gezählt werden).
- **Feld-Reiterleiste** — jedes Detailfeld, das fokussierte hervorgehoben,
  mit einem Rollanzeiger rechts, wenn das fokussierte Feld überläuft.

### Die Balken-Legende

Die `Mem`- und `CPU`-Anzeigen sind in eckige Klammern `[…]` gesetzte
Balken. Die `?`-Übersicht gibt diese Legende in der laufenden Sitzung
wieder.

Der Speicherbalken (`Mem`) ist ein **gestapelter** Balken, dessen Zellen
benennen, was der physische Speicher enthält — eine *disjunkte*
Aufteilung des belegten Speichers (`used` ist `total` minus `free`),
sodass nichts doppelt gezählt wird und die gefüllte Breite genau dem
belegten Anteil entspricht:

- `#` — nutzerresidenter Speicher (grün): Seiten, die in
  Nutzer-Adressräumen resident sind.
- `K` — der Kernel-Heap (cyan): die eigenen Heaps und Slabs des Kernels.
- `=` — sonstiger belegter Speicher (magenta): alles Belegte, das oben
  nicht zugeordnet ist (Seiten-Caches, Puffer, Kernel-Rahmen).
- leer — freier Speicher.

Der komprimierte `ramzip`-Speicher und der anonyme `pinned`-Speicher
überschneiden sich mit jenen Eimern (angeheftete Seiten sind
nutzerresident; der komprimierte Speicher ist Kernel-Speicher), daher
werden sie als nachgestellte Werte neben dem Balken statt als getrennte,
doppelt zählende Segmente ausgewiesen — ehrliche Buchführung statt eines
irreführenden Bildes.

Der Druckbalken (`Pres`) färbt jedes Band nach seiner Tiefe:
normal/gering grün, mäßig gelb, schwer/kritisch rot.

Der CPU-Balken (`CPU`) füllt sich mit belegten `#`-Zellen über einer
leeren Leerlaufspur, eingefärbt nach dem belegten Anteil (grün unter 60 %,
gelb unter 85 %, rot bei 85 % oder mehr). TAIRiX verbucht CPU-Zeit nur als
belegt gegenüber Leerlauf — es gibt keine Aufteilung in
Nutzer/System/E-A in der API — daher zeigt der Balken eine einzige
ehrliche Auslastungskategorie, mit der Detailtiefe je Kern im
`cpu`-Feld.

### Die Detailfelder

Links / Rechts (oder `p`) durchläuft sechs Felder. Jedes hat eine
invertierte Spaltenüberschrift (inverse Darstellung, fett), sodass die
Überschrift als eigener Balken über dem Rumpf lesbar ist.

### caches — das Register der rückgewinnbaren Caches

Dies sind die Caches, die der Kernel zur Entlastung von Speicherdruck
**ohne Datenverlust** zurückgeben kann: jeder Eintrag ist aus seiner
kanonischen Quelle wiederherstellbar, sodass der Kernel ihn verwirft statt
ihn auszulagern. Das Feld ist die direkte Antwort auf „tun die Caches ihre
Arbeit?“: jede Zeile ist eine Rückgewinnungsklasse, über alle
registrierten Caches aggregiert, und trägt ihre eigene **Trefferquote**.

Spalten:

- `class` — die Rückgewinnungsklasse (siehe die Klassenliste unten).
- `entries` — derzeit für die Klasse gehaltene lebende Einträge.
- `cached` — der residente Fußabdruck der Klasse: Eintragsnutzlast plus
  Buchführungsmetadaten je Eintrag, zusammen.
- `hits` — Nachschlagevorgänge der Klasse, die seit dem Start aus dem Cache
  bedient wurden (der Cache vermied die kanonische Quelle).
- `misses` — Nachschlagevorgänge der Klasse, die seit dem Start auf die
  kanonische Quelle durchfielen.
- `hit%` — die Cache-Wirksamkeitsquote, `hits / (hits + misses)` als ganze
  Prozentzahl. Eine hohe Quote heißt, dass der Cache seinen Speicher
  verdient; eine niedrige, dass er Speicher hält, ohne Arbeit zu sparen.
  Sie liest `-`, nie ein erfundenes `0%`, für eine Klasse, die in diesem
  Start nichts nachgeschlagen hat (ein untätiger Nenner).
- `ref` — seit dem Start **verweigerte** Aufnahmen (ein Eintrag, den der
  Cache abzulehnen wählte: über Budget, nicht verbuchbar oder speicherlos).
- `shr` — druckerzwungene **Schrumpf**durchläufe, die seit dem Start
  Einträge der Klasse zurückgewonnen haben.
- `fail` — interne **Fehler**, die der Klasse zugeschrieben werden: ein
  erkannter Registerdefekt, der einen Cache vergiftet (fail-closed
  deaktiviert) hat.

Zahlen werden über 99 999 als `k`/`M`/`G`/`T` abgekürzt (dezimale Tausender,
nicht KiB), damit eine Spalte sich nie verbreitert.

Die Rückgewinnungsklassen, in der Reihenfolge, in der der Kernel sie unter
Druck zurückgewinnt (die erstgenannte wird zuerst verworfen, sodass ein
Cache weiter unten in der Liste am längsten überlebt):

- `disposable-ui` — verwerfbarer Oberflächenzustand (rasterisierte Assets,
  Glyph-Atlanten, Fenster-Schnappschüsse): am billigsten zu verlieren, als
  Erstes weg.
- `predictive-prefetch` — spekulativ vorausgeladene Daten (Listen,
  Vorschaubilder, Vervollständigungsindizes): nie für die Korrektheit
  nötig.
- `background-validation` — Arbeitsergebnisse der Leerlauf-Validierung
  (Scan-Fortschritt, Kandidat-Fingerabdrücke): die spekulative Arbeit
  stoppt, sobald der Druck beginnt.
- `semantic-app-cache` — geprüfter App-Start-Zustand (geparste Manifeste,
  Validierungszusammenfassungen, Ergebnisse der Befehlsauflösung). Ihn
  zurückzugewinnen kann eine App nie unstartbar machen — das Ladegatter
  läuft einfach erneut.
- `runtime-cache` — vom Runtime gehaltener abgeleiteter Zustand
  (Lader-Vorbereitung, Ressourcenkarten): mit dem semantischen Cache
  gruppiert.
- `clean-file-data` — sauberer, wiederherstellbarer *Datei*-Inhalt, vom
  Datenträger erneut lesbar: eine begrenzte Gerätelesung baut einen Brocken
  wieder auf. Wird zurückgewonnen, bevor irgendetwas in `ramzip` komprimiert
  wird.
- `transform-cache` — teure Zwischenformen autorisierter Daten (geprüfte,
  entschlüsselte, dekomprimierte Cluster-Daten): teurer wieder aufzubauen
  als eine saubere Lesung, daher nach den sauberen Dateidaten zurückgewonnen.
- `fs-metadata` — Dateisystem-Metadaten: Statusdatensätze,
  Namensauflösungsergebnisse, Verzeichniseinträge und Sicherheitsdatensätze.
  Klein, heiß und nur durch einen mehrstufigen Baumdurchlauf wieder
  aufgebaut, daher überleben sie unter Druck die Dateidaten.
- `reliability-assist` — wiederherstellbarer Zustand der
  Wiederherstellungshilfe (Verifikationsfenster, Gesundheitszusammen-
  fassungen): durch die Wiederherstellungslatenz gerechtfertigt, daher am
  längsten bewahrt.

### ramzip — die komprimierte Speicherstufe

`ramzip` komprimiert kalte anonyme Seiten in einen kleineren Speicher im
RAM, statt sie auszulagern. Seine Abschnitte:

- `tier` — der lebende Fußabdruck: gehaltene `entries`, dargestellte
  `logical`- (unkomprimierte) Bytes, tatsächlich gehaltene `stored`-
  (Chiffretext-) Bytes und `metadata`-Buchführungsbytes; dann `saved`
  (logisch minus gespeichert) mit seinem Prozentsatz des Logischen — der
  Speicher, den die Stufe zurückgewinnt.
- `capacity` — die abgeleiteten Grenzen, auf die sich die Stufe bemisst:
  `min` (stets verfügbar), `soft` (Ziel), `hard` (Obergrenze) und die
  aktuellen `pinned`-Bytes.
- `compress` — der Speicherpfad (Schreiben): angebotene `attempts`,
  `accepted` und gespeichert, und die **Annahmequote** (angenommen /
  Versuche) — die eigene Trefferquote dieser Stufe für die Kompression.
  Darunter die Ablehnungsaufschlüsselung: nicht komprimierbar, Richtlinie,
  Grenze, ungeeignet, Reserve, Aufgabenanteil und Thrash-Verweigerungen.
- `restore` — der Abrufpfad (Lesen): Seiten-`faults`, `warm`-Wieder-
  herstellungen, `clustered`-Wiederherstellungen und ihr Gesamt
  `restored`; dann die `failures` (Authentifizierung / Dekodierung) und die
  **Erfolgsquote** (wiederhergestellt / (wiederhergestellt + Fehler)). Jede
  Quote ist ein Prozentsatz oder `-` für einen untätigen Nenner.
- `warm-up` — die `attempts` des Hintergrund-Warmwiederherstellers, seine
  `stopped`-Zahl und seine `thrash-detected`-Zahl.

### disks — Speicher eingehängter Datenträger

Eine `df`-artige Zeile je eingehängtem Datenträger: Einhängepunkt,
Dateisystemtyp, Gesamtgröße, belegt, verfügbar, Nutzungsprozentsatz und
ein ASCII-Nutzungsbalken. Ein Datenträger, dessen Treiber keine Kapazität
meldet, zeigt `capacity unknown` statt einer erfundenen Größe; ein
überraschend entfernter oder in Wiederherstellungskonflikt geratener
Datenträger wird in der Warndarstellung gezeichnet und markiert
(`[unavailable-dirty]`, `[unavailable-lost]`, `[recovery-conflict]`). Es
gibt in der API keine E-A-Durchsatzzähler je Gerät, daher sind dies
ehrliche Kapazität und Nutzung, keine erfundenen Übertragungsraten.

### cpu — Last je CPU

Eine Zeile je CPU: ihr belegter Anteil über das Intervall (`busy%`), die
Tiefe ihrer Laufwarteschlange (`queue`) und ihre Zahlen für Kontextwechsel
(`switches`) und Verdrängungen (`preemptions`) seit dem Start.

### irqs — Interrupt-Leitungen

Eine Zeile je gebundener Interrupt-Leitung, in aufsteigender
Leitungsreihenfolge: die Leitungs-ID, die besitzende Treiberaufgabe
(`owner`), die Interrupt-`count` seit dem Start und der Leitungs-`state` —
`active`, oder `quarantined` (in der Warndarstellung gezeichnet), wenn das
Sicherheitsnetz des Kernels gegen durchgehende Leitungen sie deaktiviert
hat.

### procs — die Prozesszählung

Die größten Verbraucher nach `%cpu` und nach Speicher (`size`), jeder mit
seiner pid, seinem Befehl und — für die Speichertabelle — seinem Zustand.
Die vollständige interaktive Prozessliste ist die Aufgabe von `top`; dies
ist nur die Zählungszusammenfassung.

### Fähigkeiten

Jede Zahl reist über die Systeminformations-API. Die kernelweiten
Statistikabfragen (Speicher, Druck, Caches, `ramzip`, Last je CPU) brauchen
`CAP_SYSINFO_KERNEL`; das Feld der Interrupt-Leitungen braucht
`CAP_SYSINFO_HW`; die Zählung aller Prozesse braucht `CAP_SYSINFO_GLOBAL`.
Ein Aufrufer ohne eine sieht die Ablehnung jenes Feldes ausbuchstabiert —
nie eine erfundene Zahl — während der Rest der Sitzung weiterläuft
(geschlossen scheitern, würdig abbauen). Der Speicher eingehängter
Datenträger ist ungeschützt.

## OPTIONS

- `-d, --delay <seconds>` — das Intervall zwischen automatischen
  Aktualisierungen, in Sekunden mit optionalem Bruchteil (nur die erste
  Nachkommastelle, die Zehntel, wird behalten): `sysmon -d 1.5`
  aktualisiert alle 1,5 Sekunden. Voreinstellung 3,0. GNU `top` akzeptiert
  ein Null-Intervall und aktualisiert so schnell wie möglich; TAIRiX dreht
  nie leer, daher wird eine Null auf das Minimum von 0,1 s angehoben.
- `-h, -?` — die eigene Kurzhilfe dieses Befehls zeigen und beenden.
  Innerhalb einer laufenden Sitzung schalten dieselben Tasten stattdessen
  die Tastenübersicht um.

## EXIT STATUS

- `0` — die Sitzung endete mit `q`, oder die Kurzhilfe wurde gezeigt.
- `1` — das Terminal scheiterte; der Grund wird auf die Standardfehler-
  ausgabe geschrieben.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Kennzeichen
  wie `de-DE`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
