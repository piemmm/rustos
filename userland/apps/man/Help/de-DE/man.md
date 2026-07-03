## NAME

man — das Hilfedokument eines Befehls anzeigen

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Zeigt das Hilfedokument an, das das Anwendungspaket eines Befehls
mitliefert — in Ihrer Sprache, sofern eine Übersetzung existiert.

Jedes RustOS-Programm ist ein Anwendungspaket mit einem `Help/`-Baum: ein
strukturiertes Dokument je Befehl oder Thema, je Sprache. `man` löst
`<command>` genau wie die Shell auf — zuerst der System-App-Store, dann
die Verzeichnisse in `PATH` — die angezeigte Seite dokumentiert also immer
das Programm, das die Shell für dasselbe Wort starten würde. Ein
angehängtes `.app` benennt das Paket direkt.

Das Dokument wird nach der Locale in der Umgebungsvariablen `LANG`
gewählt; ersatzweise dieselbe Sprache aus einer anderen Region und
schließlich das kanonische englische Dokument. Wird die Seite nicht in der
gewünschten Sprache angezeigt, vermerkt `man` die Ersetzung auf dem
Hinweisstrom (fd 3); die Seite selbst mischt niemals Sprachen.

Auf einer interaktiven Konsole erscheint die Seite bildschirmweise: die
Leertaste blättert weiter, die Eingabetaste rückt eine Zeile vor und `q`
beendet. Bei umgeleiteter Ausgabe oder unbekannter Konsolengröße wird die
ganze Seite ausgegeben.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `man ps` — die Seite von `ps` anzeigen.
- `man top keys` — das Thema `keys` aus dem Paket `top` anzeigen.
- `man files.app` — das Paket direkt benennen.

## EXIT STATUS

- `0` — die Seite wurde angezeigt.
- `1` — der Befehl oder sein Hilfedokument wurde nicht gefunden, oder die
  Seite konnte nicht ausgegeben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale (ein BCP-47-Kennzeichen wie `de-DE`).
- `PATH` — die zusätzlichen Verzeichnisse, in denen nach
  `<command>.app`-Paketen gesucht wird, nach dem System-App-Store.

## SEE ALSO

- `elsh`
