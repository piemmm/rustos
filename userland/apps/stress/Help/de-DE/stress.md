## NAME

stress — CPU, Speicher, Platte und Caches der Maschine gezielt belasten

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Startet Arbeitsprozesse, die die Maschine absichtlich belasten, im
Geist der etablierten Werkzeuge `stress`/`stress-ng`: CPU-Schleifen
(`--cpu`), Speicher-Anfordern-und-Anfassen (`--vm`), Schreiben/Syncen
kleiner Puffer (`--io`), große sequentielle Plattenschreiber (`--hdd`)
und Cache-aufwirbelnde Wiederleser (`--cache`, eine TAIRiX-Ergänzung).
Jeder Arbeiter ist ein eigener auslagerbarer Prozess; der steuernde
Prozess heftet seinen eigenen Speicher an (`mem_pin`, erfordert
`CAP_MEM_PIN`), damit er unter dem selbst erzeugten Druck reaktionsfähig
bleibt, und beobachtet `Strg-C`/`Terminate`, sodass jedes Ende des
Laufs — Abschluss, Zeitlimit oder Signal — die Arbeiter beendet,
einsammelt und jede Arbeitsdatei entfernt.

Speicher- und Plattenziele werden aus der Maschine selbst bemessen:
sofern `--vm-bytes`/`--hdd-bytes` keine expliziten Werte nennen, teilen
sich die vm-Arbeiter die Hälfte des erkannten RAM und die hdd-Arbeiter
die Hälfte des freien Platzes des Arbeitsdatenträgers. `--overcommit P`
skaliert diese erkannten Ziele auf `P` Prozent der Ressource; über 100
drücken die Arbeiter in den Druckbereich, und die dabei entstehenden
typisierten Ablehnungen (voller Datenträger, Ressourcenlimit) werden
als erwartete Ergebnisse gezählt und gemeldet — nie wiederholt, nie ein
Absturz. Die Maschine zu belasten braucht kein Privileg über die
eigenen Ressourcenlimits des Aufrufers hinaus — die Limits sind die
Verteidigung, und `stress` respektiert sie.

Plattenberührende Arbeiter schreiben nur unterhalb des
Arbeitsverzeichnisses — dem app-eigenen Benutzer-Cache-Verzeichnis
(`$HOME/Library/stress`), sofern `--temp-path` kein anderes nennt —
und jede Arbeitsdatei wird beim Abbau entfernt, auch auf den
Signalpfaden.

Am Ende des Laufs wird eine Zusammenfassung ausgegeben (unterdrückt
durch `--quiet`), und ein maschinenlesbarer `summary`-Datensatz wird
auf dem beratenden Standard-Informationsstrom (fd 3) ausgegeben.

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — `N` Arbeiter der
  genannten Art starten, mit der Bedeutung von GNU `stress`.
- `--cache N` — `N` Cache-Aufwirbler starten (nur TAIRiX: wiederholte
  kalte Verzeichnisläufe und Wiederlesen bewegen die
  Rückforderungs-Register des Kernels).
- `--all N` — `N` Arbeiter jeder Art.
- `--vm-bytes B`, `--hdd-bytes B` — das Byte-Ziel jedes Arbeiters,
  mit den GNU-Suffixen (`k`, `m`, `g`, `t`; z. B. `256M`).
  Voreinstellungen werden aus erkanntem RAM / freiem Platz bemessen.
- `--overcommit P` — die erkannten vm/hdd-Ziele auf `P` Prozent der
  Ressource skalieren; darf 100 überschreiten (Ablehnungen sind dann
  erwartete Ergebnisse).
- `--timeout T` — nach `T` anhalten (`s`/`m`/`h`-Suffixe; z. B.
  `5m`). Keine Voreinstellung: ohne läuft der Lauf, bis ein Signal
  ihn beendet.
- `--temp-path DIR` — das Arbeitsverzeichnis der plattenberührenden
  Arbeiter.
- `--monitor` — `sysmon` für die Dauer im Vordergrund laufen lassen;
  der Lauf wird gemeldet, wenn der Monitor endet. Widerspricht
  `--background`.
- `-q, --quiet` — die stdout-Zusammenfassung und Fortschrittszeilen
  unterdrücken (Fehler erreichen weiterhin stderr).
- `--background` — die PID des abgelösten Steuerprozesses ausgeben
  und die Eingabeaufforderung zurückgeben (impliziert `--quiet`). Die
  `&`-Job-Form der Shell funktioniert ebenso; dieses Flag ist für
  Skripte.
- `-h, -?, --help` — die eigene Kurzhilfe dieses Befehls zeigen und
  beenden.
- `--version` — Namen und Version des Werkzeugs ausgeben und beenden.

## EXIT STATUS

- `0` — der Lauf wurde abgeschlossen (typisierte Ablehnungen der
  Arbeiter sind erwartete Ergebnisse und lassen ihn nicht scheitern).
- `1` — ein Arbeiter ist tatsächlich gescheitert, oder der Lauf
  konnte nicht eingerichtet werden.
- `2` — die Befehlszeile wurde nicht verstanden.
- `130` / `143` — `Strg-C` / `Terminate` beendete den Lauf, nachdem
  die Arbeiter abgebaut und die Arbeitsdateien entfernt wurden.

## ENVIRONMENT

- `HOME` — bestimmt das voreingestellte Arbeitsverzeichnis
  (`$HOME/Library/stress`).
- `LANG` — die bevorzugte Sprache der Kurzhilfe (ein BCP-47-Kürzel
  wie `de-DE`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
