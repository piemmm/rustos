## NAME

df — Speicherplatzbelegung der Dateisysteme melden

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Meldet, eine Zeile pro eingehängtem Dateisystem, die Größe des
Datenträgers, den belegten Platz, den verfügbaren Platz, den
Belegungsanteil und den Einhängepunkt. Mit `file`-Operanden wird
stattdessen das Dateisystem gemeldet, das jeden Operanden enthält
(eine Zeile pro Dateisystem, egal wie viele Operanden es abdeckt).

Die Zahlen stammen aus der Einhängeliste der
Systeminformations-Schnittstelle, so wie jeder eingehängte
Dateisystemtreiber seine eigene Buchführung meldet. Standardmäßig
verbirgt der Bericht Einhängungen ohne eigene Kapazität (die
synthetischen Sichtbindungen des Systems) und weitere Einhängungen
eines bereits gelisteten Datenträgers; `-a` zeigt alles, und die Zahl
der verborgenen Einträge wird auf dem Standard-Informationsstrom
(fd 3) vermerkt, nie in der Tabelle.

Größen werden in 1024-Byte-Blöcken ausgegeben, sofern keine
Einheitenoption etwas anderes wählt; eine spätere Einheitenoption
überschreibt eine frühere, und Blockzahlen runden auf. Ein
Dateisystem, dessen Format Inodes bei Bedarf anlegt, meldet unter
`-i` Null-Inode-Werte — die ehrliche Antwort „nicht erfasst".

Ein `file`-Operand, der nicht existiert oder ein relativer Pfad ist
(Einhängepunkte sind absolut; `df` rät nie eine Auflösung), wird auf
der Standardfehlerausgabe gemeldet, und der Bericht fährt mit dem Rest
fort. Die GNU-Optionen `--output`, `--sync` und `--no-sync` sind noch
nicht verfügbar.

## OPTIONS

- `-a, --all` — auch die kapazitätslosen und doppelten Einhängungen
  einbeziehen, die die Voreinstellung verbirgt.
- `-T, --print-type` — die Dateisystemtyp-Spalte hinzufügen.
- `-t, --type <type>` — nur Dateisysteme des Typs `type` melden
  (wiederholbar).
- `-x, --exclude-type <type>` — Dateisysteme des Typs `type`
  auslassen (wiederholbar).
- `-i, --inodes` — Inode-Zahlen statt Blockbelegung melden.
- `-P, --portability` — das portable POSIX-Format (Kopfzeilen
  `1024-blocks` und `Capacity`).
- `-l, --local` — den Bericht auf lokale Dateisysteme beschränken
  (heute jede TAIRiX-Einhängung, es wird also nichts ausgefiltert).
- `--total` — eine mit `total` beschriftete Summenzeile anhängen.
- `-k` — 1024-Byte-Blöcke (die Voreinstellung).
- `-h, --human-readable` — menschenlesbare Größen in Zweierpotenzen
  von 1024 (`1.0K`, `23M`).
- `-H, --si` — menschenlesbare Größen in Zehnerpotenzen von 1000
  (`1.0k`, `23M`).
- `-B, --block-size <size>` — in Blöcken von `size` Bytes melden
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `df` — die Belegung jedes echten Datenträgers in
  1024-Byte-Blöcken.
- `df -h` — dasselbe in menschenlesbaren Größen.
- `df /Users/jo` — das Dateisystem, das `/Users/jo` enthält.
- `df -aT` — jede Einhängung, mit ihrem Dateisystemtyp.
- `df --total -k` — die Datenträger plus eine `total`-Summenzeile.

## EXIT STATUS

- `0` — der Bericht deckte alles Angefragte ab (oder die Kurzhilfe
  wurde ausgegeben).
- `1` — ein Operand konnte nicht gemeldet werden, die Filter ließen
  nichts übrig, oder Abfrage/Ausgabe schlugen fehl.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache für die Kurzhilfe (ein
  BCP-47-Kürzel wie `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
