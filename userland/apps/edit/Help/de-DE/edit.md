## NAME

edit — Vollbild-Texteditor

## SYNOPSIS

`edit [datei] [-h | -?]`

## DESCRIPTION

Ein Vollbild-Texteditor im Geist des klassischen QuickBasic-/
MS-DOS-Editors: eine Menüleiste oben, der Text darunter und eine
Statuszeile mit Dateinamen, Cursorposition und den wichtigsten
Tasten. Er bearbeitet eine Datei zur Zeit.

Mit einem `datei`-Operanden gestartet, lädt der Editor diese Datei;
eine noch nicht vorhandene Datei öffnet sich als leerer Puffer und
wird beim ersten Speichern angelegt. Ohne Operand gestartet, öffnet
er einen unbenannten Puffer und fragt beim ersten Speichern nach
einem Namen.

Das Menü (geöffnet mit `F10` oder mit `Alt` plus dem hervorgehobenen
Anfangsbuchstaben eines Titels — `Alt-F` für `File`, `Alt-S` für
`Search` — bewegt mit den Pfeiltasten, `Enter` wählt aus, `Esc` oder
`F10` schließt) bietet:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Würde eine Aktion ungespeicherte Änderungen verwerfen (`New`,
`Open...`, `Exit`), fragt der Editor zuerst: `y` speichert und fährt
fort, `n` verwirft, `c` (oder `Esc`) bricht ab.

Tasten in der Sitzung:

- Tippen fügt am Cursor ein; `Insert` schaltet den
  Überschreibmodus um (`OVR` in der Statuszeile).
- `Enter` teilt die Zeile; `Backspace` und `Delete` löschen Zeichen
  und verbinden Zeilen am Zeilenende.
- Pfeiltasten, `Home`, `End`, `PageUp`, `PageDown` bewegen den
  Cursor; die Ansicht rollt, auch waagerecht, hinterher.
- `Tab` fügt Leerzeichen bis zum nächsten Acht-Spalten-Stopp ein.
- `F1` zeigt die Tastenübersicht, `F2` speichert, `F3` wiederholt die
  letzte Suche, `F10` (oder `Alt-F` / `Alt-S`) öffnet das Menü.

`Find...` sucht vom Cursor aus vorwärts, wörtlich und unter
Beachtung der Groß-/Kleinschreibung, mit Umbruch am Pufferende; eine
erfolglose Suche meldet `Match not found` und lässt den Cursor
stehen.

Der Editor bearbeitet nur Textdateien und benennt genau, was er
ändert:

- Die Datei muss UTF-8-Text von höchstens 16 MiB sein; alles andere
  (eine Binärdatei, ein einzelner Wagenrücklauf, eine zu große
  Datei) wird mit Begründung abgewiesen — nie als Zeichensalat
  geöffnet.
- Tabulatoren werden beim Laden in Leerzeichen an Acht-Spalten-Stopps
  umgewandelt, CRLF-Zeilenenden werden zu LF; jede Umwandlung wird in
  der Statuszeile gemeldet, nie stillschweigend angewandt.
- Das Vorhandensein oder Fehlen des abschließenden Zeilenumbruchs
  der Datei bleibt erhalten.

Ein fehlgeschlagenes Laden oder Speichern in der Sitzung wird in der
Statuszeile gemeldet, der Puffer bleibt erhalten; die Sitzung stirbt
nie an einer abgewiesenen Datei. Jeder Pfad wird vom Kernel unter der
Identität des Aufrufers aufgelöst und geprüft — der Editor besitzt
keine besondere Autorität.

## OPTIONS

- `-h, -?` — die kurze Hilfe dieses Befehls anzeigen und beenden.

## EXIT STATUS

- `0` — die Sitzung endete über `File > Exit`, oder die kurze Hilfe
  wurde angezeigt.
- `1` — die genannte Datei konnte nicht geladen werden (kein Text, zu
  groß oder abgewiesen), oder das Terminal versagte; der Grund wird
  auf der Standardfehlerausgabe ausgegeben.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die kurze Hilfe (ein BCP-47-Tag
  wie `de-DE`).
- `TERM` — das Terminal, für das die Sitzung zeichnet; ein
  unbekannter oder fehlender Wert fällt auf eine sichere Basis
  zurück.

## SEE ALSO

- `cat`
- `man`
