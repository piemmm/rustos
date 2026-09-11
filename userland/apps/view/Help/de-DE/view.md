## NAME

view — grafischer Bild- und Dokumentbetrachter

## SYNOPSIS

`view`

## DESCRIPTION

Öffnet ein Desktop-Fenster, das ein Bild oder ein Dokument anzeigt. Mit
einem Dokument gestartet — aus der Dateiverwaltung oder durch Öffnen eines
Bildes — zeigt er diese Datei; allein gestartet bittet er die
vertrauenswürdige Dateiauswahl der Desktop-Sitzung, eine zu wählen.

Der Betrachter besitzt keine Dateisystem-Berechtigung: Er kann von sich aus
nichts öffnen, auflisten oder lesen. Die Sitzung navigiert in seinem Auftrag
unter ihrer eigenen Identität, und nur die vom Benutzer gewählte Datei wird
an ihn delegiert — einmalig und schreibgeschützt. Die Datei wird niemals im
Betrachter selbst dekodiert: Ihre Bytes werden an einen separaten
Arbeitsprozess übertragen, der überhaupt keinen Zugriff auf das Dateisystem
hat, so dass eine fehlerhafte oder böswillige Datei nichts erreichen kann,
was der Betrachter erreichen kann.

Unterstützte Formate sind JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO und
RISC OS Sprite. Eine Datei, die der Dekodierer ablehnt, nennt ihren Grund im
Fenster und auf der Standardfehlerausgabe; das Fenster bleibt niemals leer
und es wird niemals ein Bild erfunden.

Mehrere Dokumente gleichzeitig sind getrennte Betrachter: Zwei Bilder
nebeneinander zu vergleichen heißt, das zweite zu öffnen. Das Schließen
eines Fensters beendet seinen Betrachter.

Die Werkzeugleiste oben enthält der Reihe nach: verkleinern, vergrößern, ins
Fenster einpassen, Originalgröße, vorheriger Eintrag, nächster Eintrag, nach
links drehen, nach rechts drehen, spiegeln, Animation abspielen oder
anhalten, und das Informationsfeld. Ein stufenloser Zoom-Schieber sitzt an
ihrem hinteren Rand. Die Statuszeile unten nennt Name, Format, Pixelgröße,
den angezeigten Eintrag, die Länge des Dokuments und die Vergrößerung.

Ziehen Sie das Bild, um sich darin zu bewegen, wenn es größer als das
Fenster ist; solange es das ist, erscheinen Bildlaufleisten an den
Rändern der Zeichenfläche. Drehen Sie das Rad über dem Bild, um zu
verschieben. Ein sekundärer Klick auf das Bild öffnet das Menü des
Betrachters, das die Desktop-Sitzung zeichnet.

Transparenz wird auf einem Schachbrettmuster gezeigt, damit ein
transparentes Bild als transparent gelesen wird und nicht als die Farbe
dahinter.

* `+` — auf die nächste Stufe vergrößern
* `-` — auf die vorherige Stufe verkleinern
* `0` — das ganze Bild ins Fenster einpassen
* `1` — Originalgröße, ein Bildpunkt je Bildschirmpunkt
* `2` — die Breite des Bildes einpassen
* `[` / `]` — eine Vierteldrehung nach links oder rechts
* `M` — von links nach rechts spiegeln
* `I` — Informationsfeld ein- oder ausblenden
* `Space` — Animation abspielen oder anhalten
* `O` — ein anderes Dokument wählen
* `Page Up` / `Page Down` — vorheriger oder nächster Eintrag
* `Home` / `End` — erster oder letzter Eintrag
* Pfeiltasten — sich im Bild bewegen
* `Escape` — das Fenster schließen

## OPTIONS

`-h`, `-?`, `--help`
: Diese Hilfe auf die Standardausgabe schreiben und beenden.

## EXIT STATUS

Null nach einem ordentlichen Schließen. Nicht null, wenn der Fensterkanal,
der gemeinsame Rahmenbereich oder die Desktop-Sitzung abgelehnt wurde; der
Grund wird auf der Standardfehlerausgabe genannt.
