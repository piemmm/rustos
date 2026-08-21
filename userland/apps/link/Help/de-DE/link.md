## NAME

link — einer Datei einen zweiten Namen geben

## SYNOPSIS

`link [--] vorhanden neu`

## DESCRIPTION

Erzeugt eine harte Verknüpfung: `neu` wird ein zweiter Name des Knotens,
den `vorhanden` bereits benennt. Beide Namen erreichen danach dieselbe
Datei — ein Schreibvorgang über den einen ist über den anderen sichtbar,
denn es gibt eine Datei und keine Kopie — und der Speicher der Datei
überlebt, bis der letzte ihrer Namen entfernt wird.

Es gibt absichtlich keine Optionen. `ln` ist das Werkzeug mit `-f`, `-i`,
`-v`, `-s`, `-L`/`-P` und den Zielformen `-t`/`-T`; getrennt gehalten
bedeutet das, dass ein Skript, das genau eine harte Verknüpfung erzeugen
muss und nichts weiter, ein Werkzeug erhält, das keinen Namen ersetzen,
keiner Verknüpfung folgen und keine symbolische erzeugen kann.

Keiner der Namen wird verfolgt. `vorhanden` ist der Knoten **wie
geschrieben**, sodass eine dort platzierte symbolische Verknüpfung den
neuen Namen nicht auf ihr Ziel umlenken kann (`ln -L` ist das Werkzeug
für die verfolgende Haltung). `neu` ist ein Name, der erzeugt wird: ein
belegter wird abgewiesen, nie ersetzt.

Die Abweisungen sagen jeweils etwas anderes:

- der neue Name existiert schon — ein Erzeugen ersetzt nie einen Namen;
- `vorhanden` ist ein **Verzeichnis** — ein Verzeichnis hat überall
  genau einen Namen, also darf niemand ihm einen zweiten geben;
- die beiden Namen liegen auf **verschiedenen Datenträgern** — der
  zweite Name eines Knotens muss auf dem Datenträger liegen, der ihn
  speichert;
- der Namenszähler des Formats pro Knoten würde überlaufen;
- das Dateisystem speichert **einen Namen pro Knoten** — eine dauerhafte
  Eigenschaft dieses Formats, kein vorübergehender Fehler. Dort dient
  `ln -s` für eine symbolische Verknüpfung.

Genau zwei Operanden sind erforderlich; alles andere ist ein
Benutzungsfehler, und es wird keine Verknüpfung erzeugt. `--` beendet die
Optionsauswertung.

## OPTIONS

- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `link bericht.txt bericht-kopie.txt` — ein zweiter Name für eine Datei.
- `link -- -seltsamer-name zweiter` — einen Namen mit führendem
  Bindestrich verknüpfen.

## EXIT STATUS

- `0` — die Verknüpfung wurde erzeugt (oder die Kurzhilfe geschrieben).
- `1` — das Dateisystem hat die Verknüpfung abgelehnt, oder die Ausgabe
  ist fehlgeschlagen; der Grund steht auf der Standardfehlerausgabe.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein
  BCP-47-Kennzeichen wie `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
