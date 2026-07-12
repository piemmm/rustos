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
andere Byte wird als Punkt dargestellt. Der angezeigte Inhalt ist auf
den Anfang der Datei begrenzt.

`Enter` fordert eine weitere Datei an. Ein Abbruch der Auswahl lässt
den Betrachter mit einem Hinweis geöffnet. Das Schließen des Fensters
über den Desktop beendet den Betrachter.

## EXIT STATUS

Null nach sauberem Schließen; ungleich null, wenn der Fensterkanal
oder der gemeinsame Bildspeicher verweigert wurde (der Grund wird auf
dem Standardfehlerstrom ausgegeben).
