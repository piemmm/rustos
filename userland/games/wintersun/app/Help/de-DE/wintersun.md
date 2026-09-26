## NAME

wintersun — eine prozedural erzeugte Winterwelt durchwandern

## SYNOPSIS

`wintersun [--reference-scene]`

## DESCRIPTION

Öffnet ein Desktop-Fenster auf eine erzeugte Welt: die Draufsicht auf einen
Boden, den die Maschine synthetisiert statt ihn mitzuliefern, beleuchtet von
einer tief stehenden Sonne, die lange Schatten über jeden Hang wirft.

Nichts an dieser Welt ist als Bildmaterial gespeichert. Jedes Material, aus
dem der Boden besteht — Schnee, Fels, Kies, Heide, Tundra — ist eine Handvoll
Zahlen, die der Client beim Zeichnen in eine Textur verwandelt. Die Welt sieht
deshalb auf jeder Maschine gleich aus und belegt fast keinen Platz auf der
Platte. Straßen tragen sich in das ein, was sie kreuzen, statt darauf zu
liegen.

Boden, der noch nicht erzeugt wurde, wird als die Lücke gezeichnet, die er
ist, und füllt sich, sobald er eintrifft. Der Client zeichnet, was er hat,
statt anzuhalten und zu warten — das Fenster bleibt also ansprechbar, während
die Welt aufholt.

Pfeiltasten oder `W`, `A`, `S`, `D` laufen. Zwei gleichzeitig gehaltene Tasten
laufen die Diagonale dazwischen mit derselben Geschwindigkeit, und
entgegengesetzte Tasten heben sich auf. Die Ansicht folgt Ihnen und hält am
Rand der Welt an, statt darüber hinauszugleiten. Zu steile Hänge und zu tiefes
Wasser lenken Sie ab.

`+` und `-` holen die Ansicht näher und weiter weg, in fünf Stufen von einer
Weltzelle über acht Pixel bis zu einer über hundertachtundzwanzig.

`F11` schaltet das Fenster in den Vollbildmodus und gibt es danach in seinen
vorherigen Zustand zurück: ein maximiertes Fenster kommt maximiert wieder.
`Esc` stellt es wieder her. `Q` beendet.

Der Client zeichnet innerhalb eines Bildbudgets. Kann er es nicht einhalten,
gibt er Detail in einer festen Reihenfolge ab — Partikeldichte, dann die
Auflösung des Lichtpuffers, dann das Detail der Materialien, dann die
Schatten, dann die Rendergröße — und die Bildrate ist nie das, was nachgibt.
Jede Stufe wird zurückgegeben, sobald die Bilder eine Weile bequem waren. Die
Reihenfolge liegt fest, damit das Ergebnis auf einer langsamen Maschine
vorhersehbar ist statt überraschend.

Ein Fenster, das größer ist, als der Software-Renderer füllen kann, wird mit
höchstens 2560×1440 gezeichnet und auf die Fenstergröße hochskaliert.

## OPTIONS

- `-h, -?, --help` — die Kurzhilfe dieses Befehls anzeigen.
- `--reference-scene` — die feste Referenzszene zeichnen und stillhalten: eine
  Welt, dieselben Figuren und derselbe Augenblick, auf jeder Maschine gleich,
  damit ein Bild des Fensters mit einem anderswo gezeichneten verglichen
  werden kann. `F11` und `Esc` ändern weiterhin die Fenstergröße; sonst bewegt
  sich nichts.

## EXIT STATUS

`0`, wenn Sie beenden. Ein Status ungleich null nennt seinen Grund auf der
Standardfehlerausgabe: die Welt konnte nicht erzeugt werden, das Fenster
konnte nicht geöffnet werden, oder der Ereigniskanal der Sitzung ging
verloren.

- `2` — die Befehlszeile wurde nicht verstanden.
- `87` — die Referenzszene konnte nicht gezeichnet werden.

## SEE ALSO

`sapper`
