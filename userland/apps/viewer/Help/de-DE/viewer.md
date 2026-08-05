## NAME

viewer — grafischer schreibgeschützter Dateibetrachter

## SYNOPSIS

`viewer`

## DESCRIPTION

Öffnet ein Desktop-Fenster und bittet sofort die vertrauenswürdige
Dateiauswahl der Desktop-Sitzung, eine Datei zu wählen. Der Betrachter
selbst besitzt keine Dateisystem-Berechtigung: Er kann von sich aus
nichts öffnen, auflisten oder lesen. Die Sitzung navigiert im Auftrag
des Betrachters unter ihrer eigenen Identität, und nur die eine vom
Benutzer gewählte Datei wird an den Betrachter delegiert — einmalig
und schreibgeschützt.

Der Inhalt der gewählten Datei wird als Klartext vom oberen Rand des
Fensters angezeigt. Druckbare Zeichen erscheinen unverändert; jedes
andere Byte wird als Punkt dargestellt, so dass binärer Inhalt
offensichtlich bereinigt erscheint. Der angezeigte Inhalt ist auf
den Anfang der Datei begrenzt.

Das Fenster wird mit der Maus gesteuert. Klicken Sie auf die
Schaltfläche **Open…** (Öffnen…) in der Kopfzeile, um eine andere Datei
anzufordern. Ziehen Sie den Schieber der Bildlaufleiste nach oben oder
unten, um durch eine lange Datei zu navigieren, klicken Sie auf die
Leiste ober- oder unterhalb des Schiebers, um seitenweise zu blättern,
klicken Sie auf die Endschaltflächen, um zeilenweise zu springen, oder
drehen Sie das Rad über dem Fenster, um zu scrollen. Ein Abbruch der
Auswahl lässt den Betrachter mit einem Hinweis geöffnet; das Schließen
des Fensters über den Desktop beendet den Betrachter.

Die Tastatur ist ein sekundärer Pfad für dieselben Aktionen: `Enter`
fordert eine weitere Datei an, die Pfeiltasten springen eine Zeile
weiter, Bild-auf/Bild-ab springen eine Seite weiter und Home/End
springen an den Anfang oder das Ende.

## EXIT STATUS

Null nach sauberem Schließen; ungleich null, wenn der Fensterkanal
oder der gemeinsame Bildspeicher verweigert wurde (der Grund wird auf
dem Standardfehlerstrom ausgegeben).
