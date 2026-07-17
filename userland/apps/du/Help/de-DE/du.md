## NAME

du — Speicherplatzverbrauch von Dateien schätzen

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Durchläuft jeden `file`-Operanden und gibt pro Verzeichnis (tiefste
zuerst) den Speicherplatz aus, den der darunterliegende Baum belegt,
als `size<TAB>path`. Ohne `file` wird das aktuelle Verzeichnis (`.`)
durchlaufen. Ein `file`-Operand, der kein Verzeichnis ist, wird für
sich allein ausgegeben.

Das Standardmaß ist der tatsächlich belegte Speicher jedes Knotens,
wie ihn das eingehängte Dateisystem meldet; dünn besetzte oder
komprimierte Dateien zählen also, was sie wirklich belegen.
`--apparent-size` (oder `-b`) misst stattdessen die scheinbaren
Bytelängen. Größen werden in 1024-Byte-Blöcken ausgegeben, sofern
keine Einheitenoption etwas anderes wählt; eine spätere
Einheitenoption überschreibt eine frühere, und Blockzahlen runden auf
(ein teilweise genutzter Block ist ein genutzter Block).

Ein unlesbarer Pfad wird auf der Standardfehlerausgabe gemeldet, und
der Lauf fährt mit dem Rest fort; ein unlesbares Verzeichnis trägt
nichts bei statt einer geratenen Teilsumme.

TAIRiX hat noch keine harten Verknüpfungen, daher kann kein Eintrag
doppelt gezählt werden und die GNU-Schalter zur
Verknüpfungs-Deduplizierung existieren nicht; `-x` (ein Dateisystem)
ist noch nicht verfügbar; die Umgebungsvariablen der
`DU_BLOCK_SIZE`-Familie werden nicht gelesen — die Skala wird allein
durch Optionen gewählt.

## OPTIONS

- `-a, --all` — auch jede Datei ausgeben, nicht nur Verzeichnisse.
- `-s, --summarize` — nur das Total jedes Operanden ausgeben (steht im
  Konflikt mit `-a` und `-d`).
- `-c, --total` — eine mit `total` beschriftete Gesamtzeile anhängen.
- `-d, --max-depth <n>` — Verzeichnisse höchstens `n` Ebenen unter
  einem Operanden ausgeben (`0` gibt nur die Operanden aus); Totale
  bleiben unberührt.
- `-S, --separate-dirs` — die Zeile eines Verzeichnisses schließt
  seine Unterverzeichnisse aus.
- `--apparent-size` — scheinbare Bytelängen messen, nicht belegten
  Speicher.
- `-b, --bytes` — scheinbare Größe in einzelnen Bytes
  (`--apparent-size` mit Blockgröße 1).
- `-k` — 1024-Byte-Blöcke (die Voreinstellung).
- `-m` — 1048576-Byte-Blöcke.
- `-h, --human-readable` — menschenlesbare Größen in Zweierpotenzen
  von 1024 (`1.0K`, `23M`).
- `--si` — menschenlesbare Größen in Zehnerpotenzen von 1000 (`1.0k`,
  `23M`).
- `-B, --block-size <size>` — in Blöcken von `size` Bytes ausgeben
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — jede Zeile mit NUL statt Zeilenumbruch beenden.
- `-?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `du` — der Baum des aktuellen Verzeichnisses, eine Zeile pro
  Verzeichnis.
- `du -sh /Users/jo` — ein menschenlesbares Total für `/Users/jo`.
- `du -a docs` — jede Datei und jedes Verzeichnis unter `docs`.
- `du -d1 -c /Apps /Users` — die erste Ebene jedes Speichers, dann ein
  Gesamttotal.

## EXIT STATUS

- `0` — jeder Operand wurde durchlaufen (oder die Kurzhilfe wurde
  ausgegeben).
- `1` — ein Pfad konnte nicht gelesen oder die Ausgabe nicht
  zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache für die Kurzhilfe (ein BCP-47-Kürzel
  wie `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
