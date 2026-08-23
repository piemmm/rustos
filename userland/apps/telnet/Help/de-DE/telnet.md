## NAME

telnet — der Netzwerk-Virtual-Terminal-Client nach RFC 854

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Öffnet eine TCP-Verbindung zu einem Rechner und leitet das Terminal dorthin
weiter: die Ausgabe des Gegenübers erscheint auf der Standardausgabe,
Tastatureingaben gehen an den Rechner, und das Fluchtzeichen (`^]`
standardmäßig) öffnet den Kommandointerpreter `telnet>`. Ohne Rechnernamen
startet `telnet` an dieser Eingabeaufforderung, und `open` verbindet.

Es ist sowohl der Weg zu einem zeilenorientierten Dienst auf einer anderen
Maschine als auch der Weg, einen beliebigen TCP-Dienst von Hand anzusprechen —
`telnet host 80` öffnet eine Verbindung, in die eine Anfrage getippt werden
kann.

Der Rechner darf ein Name oder eine literale IPv4-/IPv6-Adresse sein. Ein Name
wird über den Stub-Resolver des Systems aufgelöst, der die konfigurierten
rekursiven DNS-Server über die Systeminformations-API liest. Der Port ist eine
Zahl: es gibt keine Dienstedatenbank, ein Dienst*name* ist daher ein
Benutzungsfehler und kein stiller Rückfall auf Port 23.

Die Optionsverhandlung folgt RFC 855 mit der schleifenfreien Disziplin aus
RFC 1143, sodass ein sich wiederholendes Gegenüber den Client nie zum
Wiederholen bringt. Implementiert sind BINARY, ECHO, SUPPRESS GO AHEAD,
STATUS, TIMING MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW
CONTROL, LINEMODE und NEW-ENVIRON; alles andere wird abgelehnt, was genau das
bedeutet, was eine nicht implementierte Option bedeutet. LINEMODE (RFC 1184)
ist vollständig umgesetzt — die `MODE`-Maske, die SLC-Zeichentabelle und
`FORWARDMASK` — sodass der Client die Zeile so bearbeitet, wie der Server es
verlangt, mit den Zeichen, die der Server verhandelt.

Die Fenstergröße wird über NAWS beim Verbinden und bei jeder Änderung
gemeldet. TAIRiX kennt kein Signal für Größenänderungen, daher wird die Größe
bei jedem Tastendruck neu gelesen; eine Änderung erreicht den Rechner also
beim nächsten Tastendruck.

`NEW-ENVIRON` gibt **nur** Variablen weiter, die mit dem Befehl `environ`
definiert und exportiert wurden; der Client sendet seine eigene Umgebung
niemals. `-a` und `-l` exportieren einen Anmeldenamen — das Einzige, was ein
Aufruf von sich aus offenlegt.

Zwei Befehle des historischen Werkzeugs fehlen bewusst. Es gibt kein
`!`-Shell-Escape: einem Programm, das feindliche Netzwerkdaten auswertet, wird
nicht das Recht gegeben, eine Shell zu starten. Es gibt kein `slc check`, denn
RFC 1184 gibt ihm keine von `slc export` unterscheidbare Form auf der
Leitung. TCP-Vorrangdaten stellt die Socket-Schnittstelle nicht bereit, daher
reist ein Synch als bloße Data Mark. Erreicht die Standardeingabe das
Dateiende — ein umgeleiteter Aufruf wie `telnet host 80 < anfrage` — wird nur
die Senderichtung geschlossen und die Sitzung liest weiter, bis auch der
entfernte Rechner schließt; die Antwort wird also nicht verworfen, wie es das
historische Werkzeug tut.

## OPTIONS

- `-4, --ipv4` — nur über IPv4 verbinden.
- `-6, --ipv6` — nur über IPv6 verbinden.
- `-8, --binary` — einen 8-Bit-Datenpfad in beiden Richtungen anfordern.
- `-L, --eight-bit-output` — einen 8-Bit-Datenpfad nur für die Ausgabe.
- `-E, --no-escape` — kein Fluchtzeichen; jeder Tastendruck geht an den Rechner.
- `-e, --escape <char>` — das Fluchtzeichen setzen (`^]`, `^A`, ein einzelnes
  Zeichen, oder leer für keines).
- `-a, --login` — den Anmeldenamen der Sitzung über `NEW-ENVIRON` exportieren.
- `-l, --user <name>` — `name` als Anmeldenamen exportieren (impliziert `-a`).
- `-b, --bind <address>` — diese lokale Adresse vor dem Verbinden binden.
- `-d, --debug` — die Optionsverhandlung auf der Standardfehlerausgabe
  mitschreiben.
- `-?, --help` — die kurze Hilfe dieses Befehls anzeigen.

## EXAMPLES

- `telnet example.test` — eine Sitzung auf dem zugewiesenen Telnet-Port öffnen.
- `telnet 10.0.2.2 25` — von Hand mit einem Mail-Dienst sprechen.
- `telnet -6 fe80::2` — nur über IPv6 verbinden.
- `telnet -l ada host` — `ada` als Anmeldenamen anbieten.
- `telnet -8 host` — einen 8-Bit-Pfad in beiden Richtungen anfordern.
- `telnet`, dann `open host` — von der Eingabeaufforderung aus verbinden.

## EXIT STATUS

- `0` — die Sitzung fand statt (wie auch der Rechner sie beendete), oder die
  kurze Hilfe wurde geschrieben.
- `1` — die Sitzung war nicht möglich: der Rechner wurde nicht aufgelöst, der
  Socket wurde abgelehnt, oder das Terminal ließ sich nicht in den Rohmodus
  schalten.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `TERM` — dem Rechner über die Option TERMINAL TYPE gemeldet.
- `USER` — der Anmeldename, den `-a` exportiert.
- `LANG` — die bevorzugte Locale für die kurze Hilfe (ein BCP-47-Tag wie
  `de-DE`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
