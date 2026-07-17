## NAME

top — die Prozessliste live beobachten

## SYNOPSIS

`top [-d Sek.Zehntel] [-h | -?]`

## DESCRIPTION

Zeigt eine live aktualisierte Vollbildansicht der Prozessliste über die
Systeminformations-API, im Geiste des klassischen `top`. Es startet mit
den Prozessen des Aufrufers; die systemweite Sicht gewährt der Dienst
nur einem Aufrufer mit `CAP_SYSINFO_GLOBAL`.

Die Anzeige frischt sich in jedem Intervall selbst auf (3,0 Sekunden,
sofern `-d` nichts anderes bestimmt), und `r` frischt sie sofort auf.

Der Betrachter nimmt keine Operanden an: er wird mit Tasten innerhalb
der Sitzung gesteuert.

- `q` — beenden.
- `a` — zwischen den eigenen Prozessen und der systemweiten Sicht
  umschalten. Verweigert der Dienst die systemweite Sicht (sie
  erfordert `CAP_SYSINFO_GLOBAL`), bleibt der Betrachter bei den
  eigenen Prozessen und die Statuszeile nennt den Grund; die Sitzung
  läuft weiter.
- `r` — die Liste auffrischen.
- Hoch/Runter, BildAuf/BildAb, Pos1/Ende — die Auswahl bewegen.
- `h`, `?` — die Tastenübersicht ein- oder ausblenden.

Vier Übersichtszeilen stehen über der Liste: die Laufzeit, die Zahl der
angemeldeten Benutzer und die 1/5/15-Minuten-Lastmittel; die Zählung der
Tasks nach Zustand; die `%Cpu(s)`-Auslastungsaufteilung; und die
Speicherwerte in MiB. Die Speicherzeile erfordert `CAP_SYSINFO_KERNEL` —
ein Aufrufer ohne diese Berechtigung sieht die Ablehnung ausgeschrieben,
und die Sitzung läuft weiter.

Die `%Cpu(s)`-Zeile zeigt den Anteil des letzten Intervalls, den alle
CPUs zusammen beschäftigt (mit dem Ausführen von Tasks) und untätig
verbracht haben. TAIRiX verbucht nur Beschäftigt- und Leerlaufzeit: wo
GNU `top` den beschäftigten Anteil in user/system/nice/iowait
aufschlüsselt, zeigt diese Zeile bewusst die zwei echten Werte.

Die Zeilen sind nach `%CPU` sortiert, der größte Verbraucher zuerst, und
tragen:

- `PID` — die numerische Prozesskennung.
- `USER` — der Benutzername des besitzenden Kontos, aufgelöst aus dem
  Kontoverzeichnis des Systems; die numerische uid tritt an die Stelle,
  wenn der Name nicht aufgelöst werden kann.
- `SIZE` — der im Adressraum des Prozesses eingeblendete Speicher
  (Abbild, Stapel und Halde gleichermaßen).
- `S` — der Zustandsbuchstabe: `R` laufend (grün), `r` lauffähig,
  wartet auf eine CPU (cyan), `S` schlafend, `T` angehalten (gelb), `Z`
  Zombie (magenta). Farben erscheinen nur auf einem Farbterminal; der
  Buchstabe trägt den Zustand immer.
- `%CPU` — der CPU-Anteil über das Intervall seit der letzten
  Auffrischung.
- `WCPU` — der gewichtete (exponentiell geglättete) CPU-Anteil über die
  Auffrischungen hinweg, ruhiger als die Momentanspalte.
- `TIME+` — die kumulierte CPU-Zeit als
  `Minuten:Sekunden.Hundertstel`.
- `COMMAND` — der Prozessname.

## OPTIONS

- `-d, --delay <seconds>` — das Intervall zwischen automatischen
  Auffrischungen, in Sekunden mit optionalem Bruchteil (nur die erste
  Nachkommastelle, die Zehntel, wird behalten): `top -d 1.5` frischt
  alle 1,5 Sekunden auf. Vorgabe ist 3,0. GNU `top` akzeptiert null und
  frischt so schnell wie möglich auf; TAIRiX läuft nie in einer
  Beschäftigungsschleife, daher wird null auf das Minimum von 0,1 s
  angehoben.
- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen und beenden. In
  einer laufenden Sitzung schalten dieselben Tasten stattdessen die
  Tastenübersicht um.

## EXIT STATUS

- `0` — die Sitzung endete mit `q`, oder die Kurzhilfe wurde angezeigt.
- `1` — der Dienst oder das Terminal versagte; der Grund wird auf der
  Standardfehlerausgabe ausgegeben.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `man`
- `ps`
- `sysinfo`
