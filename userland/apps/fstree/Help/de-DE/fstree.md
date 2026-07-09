## NAME

fstree — der Vollbild-Dateimanager mit Verzeichnisbaum

## SYNOPSIS

`fstree [verzeichnis]`

## DESCRIPTION

Durchsucht das Dateisystem in einer tastaturgesteuerten Vollbildsitzung:
links ein Verzeichnisbaum, rechts ein Dateibereich, der die Einträge des
ausgewählten Verzeichnisses mit Größe und Änderungszeit auflistet. Die
Sitzung beginnt in `verzeichnis` (ohne Angabe in der Wurzelansicht `/`).

Der Baum wird verzögert gelesen: Der Inhalt eines Verzeichnisses wird
erst geholt, wenn es zum ersten Mal angezeigt oder aufgeklappt wird —
das Durchstöbern eines riesigen Datenträgers kostet also nur die
tatsächlich geöffneten Verzeichnisse. Ein Verzeichnis, das der Aufrufer
nicht auflisten darf, wird an Ort und Stelle verweigert: Der Fehler
erscheint in der Meldungszeile, die vorherige Ansicht bleibt erhalten;
nichts wird erfunden.

Tasten:

- `Hoch`/`Runter` oder `k`/`j` — den Cursor des aktiven Bereichs bewegen.
  Bewegt sich der Baumcursor, wird das neu ausgewählte Verzeichnis im
  Dateibereich aufgelistet.
- `Links`/`Rechts` oder `h`/`l` — die Baumzeile unter dem Cursor
  zu-/aufklappen.
- `Eingabe` — im Baum das Aufklappen umschalten; im Dateibereich in das
  ausgewählte Verzeichnis hinabsteigen (beide Bereiche folgen).
- `Tab` — den aktiven Bereich wechseln.
- `s` — das Sortiermenü öffnen: `n` Name, `e` Erweiterung, `s` Größe,
  `m` Änderungszeit, `r` Richtung umkehren, `Esc` bricht ab.
  Verzeichnisse stehen stets vor den Dateien.
- `c` — den gewählten Eintrag kopieren: eine Eingabezeile fragt nach dem
  Ziel. Ein relatives Ziel landet im aufgelisteten Verzeichnis; ist das
  Ziel ein bestehendes Verzeichnis, wird die Kopie darin unter dem Namen
  der Quelle abgelegt. Ein Verzeichnis wird mit allem darunter kopiert.
  Das Kopieren eines Eintrags auf sich selbst oder eines Verzeichnisses
  in seinen eigenen Teilbaum wird verweigert, bevor etwas geschrieben
  wird.
- `m` — den gewählten Eintrag verschieben, mit derselben Zielabfrage.
  Innerhalb eines Datenträgers ist das Verschieben ein atomares
  Umbenennen; über Datenträgergrenzen wird kopiert und danach die Quelle
  entfernt.
- `r` — den gewählten Eintrag an Ort und Stelle umbenennen: die
  Eingabezeile ist mit dem aktuellen Namen vorbelegt.
- `d` — den gewählten Eintrag nach einer Rückfrage löschen; nur `y`
  fährt fort. Das Löschen eines Verzeichnisses entfernt alles darunter,
  und die Rückfrage sagt das.
- `M` — ein Verzeichnis im aufgelisteten Verzeichnis anlegen; der Name
  wird abgefragt.
- `a` — die Berechtigungsbits des gewählten Eintrags bearbeiten: eine
  oktale Eingabezeile, vorbelegt mit dem aktuellen Modus. Enter wendet an
  (nur der Eigentümer darf ändern — der Kernel weist alle anderen ab),
  Esc bricht ab.
- `t` — den gewählten Eintrag des Dateibereichs markieren oder die
  Markierung aufheben und eine Zeile hinabgehen; wiederholtes Drücken
  markiert also eine Reihe. Markierte Einträge tragen ein `*`.
- `T` — nach Muster markieren: ein Glob (`*`, `?`, `[...]`), der gegen
  die sichtbaren Namen geprüft wird; jeder Treffer kommt zur
  Markierungsmenge hinzu.
- `i` — die Markierungen über die sichtbaren Einträge invertieren.
- `C` — alle Markierungen aufheben.
- `u` — den Plattenverbrauch unter dem fokussierten Verzeichnis
  zählen: Dateien, Bytes und Verzeichnisse, schrittweise im
  Hintergrund durchlaufen. `Esc` bricht ab und behält die bis dahin
  gezählten Zahlen.
- `v` — den Zweig unter dem fokussierten Verzeichnis flach anzeigen:
  eine Liste jeder Datei darunter, seitenweise gefüllt (`Leertaste`
  lädt die nächste Seite). In der Ansicht markieren `t`/`T`/`i`/`C`
  ihre Zeilen, `c`/`m`/`d` führen Stapeloperationen über die
  Markierungsmenge aus, und `Esc` kehrt zu den Bereichen zurück. Die
  Zeilen sind relativ zum flachen Zweig benannt.
- `.` — versteckte Einträge (Punktnamen) in beiden Bereichen ein- und
  ausblenden.
- `?` — diese Hilfe über den Bereichen anzeigen; jede Taste schließt sie.
- `q` — beenden und das Terminal wiederherstellen.

Solange Einträge markiert sind, wirken `c`, `m` und `d` auf die ganze
Markierungsmenge statt auf die Auswahl: `c`/`m` fragen nach einem
bestehenden Zielverzeichnis, in dem die Einträge landen, und `d`
bestätigt das Stapellöschen. Die Einträge werden in
Markierungsreihenfolge verarbeitet; ein fehlgeschlagener Eintrag hält
den Rest nie auf, der Abschlussbericht zählt das Gelungene, und ein
Berichtsbildschirm nennt jeden Fehlschlag beim Namen — ein Stapel ist
nie stillschweigend unvollständig. Gelungene Einträge verlieren ihre
Markierung; Fehlschläge bleiben für einen neuen Versuch markiert.

Würde ein Kopieren oder Verschieben eine bestehende Datei
überschreiben, fragt die Sitzung pro Datei: `o` überschreibt, `s`
überspringt (eine übersprungene Quelle bleibt an ihrem Platz), und `c`
bricht die verbleibenden Schritte ab — in einem Stapel verwirft der
Abbruch alle verbleibenden Einträge — bereits Angewandtes bleibt
bestehen, und der Abschlussbericht sagt, was geschah. Ein Fehler
mitten im Kopieren entfernt das halb geschriebene Ziel und zeigt den
Fehler des Kernels; nichts gibt sich je als vollständige Kopie aus.
Jede Operation wird vom Kernel autorisiert — eine Verweigerung
erscheint wörtlich in der Meldungszeile, ohne dass sich etwas ändert.

Die Statuszeile zeigt den aufgelisteten Pfad, die Zahl der sichtbaren
Einträge, die Sortierordnung, die freien/gesamten Bytes des tragenden
Datenträgers (sofern der Systeminformationsdienst sie melden kann), ob
versteckte Einträge angezeigt werden und — solange etwas markiert ist —
die Zahl der Markierungen samt Bytesumme. Eine Datei, deren Format keine
Änderungszeit speichert, zeigt `-` in der Zeitspalte.

Die Suche und die Text-/Hex-/Disassembler-Ansichten
kommen in späteren Stufen des Plans dieses Werkzeugs.

## OPTIONS

- `directory` — das Startverzeichnis der Sitzung; Vorgabe ist die
  Wurzelansicht `/`.
- `-h`, `-?` — die Kurzform dieses Dokuments ausgeben und beenden.

## EXIT STATUS

- `0` — die Sitzung endete durch das `q` des Benutzers.
- `1` — das Startverzeichnis konnte nicht aufgelistet werden, oder der
  Terminalpfad schlug fehl.
- `2` — die Argumente konnten nicht verstanden werden.

## SEE ALSO

ls, cp, mv, rm, mkdir, chmod, du, df, find
