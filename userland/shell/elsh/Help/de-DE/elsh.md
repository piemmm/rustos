## NAME

elsh — die TAIRiX-Befehlsshell

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Startet eine interaktive Befehlsshell — eine
Lese-Auswerte-Ausgabe-Schleife über die geerbten Standardströme. Ein
getipptes Befehlswort wird zuerst gegen die eingebauten Befehle der
Shell aufgelöst, dann im System-App-Store (`/System/Apps`), dann in den
Verzeichnissen der Variablen `PATH`; der Store wird vor `PATH`
durchsucht, sodass `PATH` nie einen Systembefehl überdecken kann. Ein
nicht aufgelöstes Wort endet mit `127`; ein aufgelöstes, aber nicht
ausführbares Bundle endet mit `126`.

Die eingebauten Befehle:

- `cd <path>`, `pwd` — das Arbeitsverzeichnis wechseln und ausgeben.
- `echo ...` — die eigenen Operanden ausgeben.
- `export NAME=value`, `unset NAME` — die exportierte Umgebung
  bearbeiten.
- `jobs`, `fg`, `bg` — Jobkontrolle.
- `ulimit` — Ressourcenlimits lesen und setzen.
- `elevate` — einen Befehl über den Anmelde-Supervisor der Konsole
  neu authentifiziert ausführen.
- `help` — die eingebauten Befehle auflisten.
- `exit [code]` — die Sitzung beenden.

Die Shell nimmt keine Operanden an: die Ausführung von Skripten gehört
noch nicht zu ihrer Grammatik.

Auf einem Terminal bietet die Shell einen interaktiven Zeileneditor:
Pfeil-hoch/-runter blättern durch den Befehlsverlauf, `Ctrl-R`
durchsucht ihn, `Ctrl-C` verwirft die aktuelle Zeile, `Ctrl-D` auf
leerer Zeile beendet die Sitzung, und Tab vervollständigt Befehlsnamen,
Dateipfade und Ressourcenreferenzen wie `sys:random`.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen und beenden.

## EXIT STATUS

- Der Code des eingebauten `exit`, oder `0`, wenn der Eingabestrom
  endet (oder die Kurzhilfe angezeigt wurde).
- `2` — der Aufruf wurde nicht verstanden.

## ENVIRONMENT

- `PATH` — die nach dem System-App-Store durchsuchten Verzeichnisse.
- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`), exportiert an jeden gestarteten Befehl.

## SEE ALSO

- `man`
