## NAME

applib — die Programmbibliothek des Desktops verwalten

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Verwaltet die Programmbibliothek — den in Ordnern organisierten Katalog
startfähiger Anwendungen, den der Launcher des Desktops präsentiert.
Die Bibliothek besteht aus Daten auf dem Datenträger, niemals aus einer
fest eincompilierten Liste: ein systemweiter Speicher unter
`/System/Settings/ProgramLibrary/library.conf`, den jedes Konto liest,
plus ein optionales benutzerbezogenes Overlay unter demselben Pfad
innerhalb der eigenen `Settings/` des Benutzers. Was ein Launcher
anzeigt, ist das Ergebnis der Zusammenführung beider: Die eigenen
Einträge und Anpassungen des Benutzers gewinnen gegenüber den
systemweiten.

Ohne Unterbefehl (oder mit `list`) wird die aufgelöste Bibliothek
Ordner für Ordner ausgegeben, ein Eintrag pro Zeile: Kennung,
Anzeigename und Paketpfad — genau das, was der Launcher anzeigt. Die
Ordner sind die abgeschlossene Menge `Accessories`, `Graphics`,
`Internet`, `Multimedia`, `Office`, `Programming`, `Games`,
`SystemTools`, `Utilities` und `Other`; es gibt keine frei wählbaren
Ordner.

`applib add` registriert ein Anwendungspaket. Identität, Anzeigename,
Ordner und Symbol werden aus dem eigenen signierten `AppInfo`-Manifest
des Pakets übernommen; `--category`, `--name` und `--icon`
überschreiben das Manifest. Ein Paket, dessen Manifest keinen
Bibliotheksordner deklariert, benötigt eine explizite `--category` —
das Werkzeug rät niemals. `applib remove` entfernt einen Datensatz,
benannt nach seiner Kennung oder nach dem Paketpfad, mit dem er
registriert wurde.

`applib hide` unterdrückt einen Eintrag in der aufgelösten Bibliothek,
ohne seinen Datensatz zu löschen — seine Kennung bleibt beansprucht, so
dass ein späterer `rescan` ihn nicht wiederbeleben kann — und
`applib show` zeigt ihn wieder an. Das Ausblenden betrifft nur die
Darstellung, niemals die Berechtigung: Das Starten eines Pakets wird
unabhängig vom Katalog weiterhin durch die Signatur- und
Capability-Prüfungen des Loaders geregelt.

`applib rescan` durchsucht die Anwendungsspeicher (`/System/Apps` und
`/Apps` oder das eigene `<home>/Apps` des Aufrufers bei `--user`),
liest das Manifest jedes Pakets und registriert jede Anwendung, die um
Listung bittet und noch nicht katalogisiert ist. Bestehende
Datensätze — einschließlich Umbenennungen und Unterdrückungen durch
einen Kurator — werden niemals gestört, und ein Paket mit einem
unleserlichen oder fehlerhaften Manifest wird übersprungen und
gezählt, niemals ein Grund zum Abbruch. So bevölkert sich die
Bibliothek eines frischen Systems selbst aus den tatsächlich
installierten Paketen, ohne handgeführte Liste an irgendeiner Stelle.

Standardmäßig bearbeitet das Werkzeug den systemweiten Speicher, den
nur ein durch die Schreibrichtlinie von `/System/Settings` zugelassener
Prinzipal ändern kann; ein gewöhnliches Konto liest ihn,
personalisiert aber durch sein eigenes Overlay mit `--user`. Eine
abgelehnte Schreiboperation gibt ihren Grund an und ändert nichts.

Bei Erfolg verhält sich das Werkzeug auf der Standardausgabe still; das
Ergebnis einer Änderung wird als strukturierter Hinweisdatensatz auf
dem Standard-Informationsstrom (fd 3) ausgegeben, den Skripte mit
`3>records.jsonl` erfassen können und den alles andere ignorieren kann.

## OPTIONS

- `--category <folder>` — mit `list`: nur diesen Ordner anzeigen; mit
  `add`: den Eintrag darunter einordnen (überschreibt die Deklaration
  des Manifests).
- `--name <name>` — mit `add`: der Anzeigename, der anstelle des
  Namens im Manifest angezeigt werden soll.
- `--icon <asset>` — mit `add`: das Symbol-Asset (ein Dateiname
  innerhalb des `Resources/`-Ordners des Pakets) anstelle des Symbols
  im Manifest.
- `--user` — die Änderung auf das eigene Overlay des Aufrufers
  anwenden (oder, bei `rescan`, das eigene `<home>/Apps` des Aufrufers
  durchsuchen) anstelle des systemweiten Speichers.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `applib` — die aufgelöste Bibliothek Ordner für Ordner anzeigen.
- `applib list --category Games` — einen einzelnen Ordner anzeigen.
- `applib add /Apps/chess.app` — ein Paket so registrieren, wie es sein
  Manifest verlangt.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  ein Paket, das keine Listung deklariert, unter einem expliziten
  Ordner registrieren.
- `applib remove os.tairix.chess` — einen Eintrag nach Kennung
  entfernen.
- `applib hide os.tairix.chess --user` — ihn nur aus der eigenen
  Bibliothek ausblenden.
- `applib rescan` — jedes installierte, gelistete Paket registrieren,
  das noch nicht im Systemkatalog enthalten ist.

## EXIT STATUS

- `0` — Listung, Änderung, Rescan oder Kurzhilfe wurden abgeschlossen.
- `1` — ein Speicher-, Paket- oder Ausgabefehler (z. B. darf der
  Aufrufer den systemweiten Katalog nicht ändern); der Grund wird auf
  dem Diagnosestrom angegeben.
- `2` — die Befehlszeile wurde nicht verstanden, der Ordner oder
  Eintrag ist unbekannt oder das Paket kann nicht wie gewünscht
  registriert werden.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache für die Kurzhilfe (ein
  BCP-47-Kennzeichen wie `fr-FR`).
- `HOME` — das Home-Verzeichnis des Aufrufers: benennt das
  benutzerbezogene Overlay und die `--user`-Rescan-Wurzel
  `<home>/Apps`.

## SEE ALSO

- `man`
- `configure`
