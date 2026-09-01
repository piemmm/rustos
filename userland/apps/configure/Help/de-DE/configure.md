## NAME

configure — die Systemkonfiguration zum Startzeitpunkt lesen und setzen

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Listet, zeigt und setzt die Einstellungen des Konfigurationsspeichers
unter `/System/Settings/Configuration/system.conf`. Ohne Operand wird
jede Einstellung mit ihrem aktuellen Wert aufgelistet; mit einem
Schlüssel allein wird dessen Wert angezeigt; mit Schlüssel und Wert
wird die Einstellung geändert.

Der Speicher liegt auf dem verschlüsselten Root-Datenträger und wird
von seinen Verbrauchern nach dem Entsperren des Root-Dateisystems
gelesen; eine Änderung wirkt daher beim nächsten Start ihres
Verbrauchers (`os.loginType`: die Anmeldung des nächsten
Systemstarts; die `cache.*`-Schalter: das Entsperren des nächsten
Systemstarts).

Die Schlüsselmenge ist geschlossen: ein unbekannter Schlüssel oder ein
Wert außerhalb der Menge eines Schlüssels wird mit Angabe der gültigen
Auswahl abgelehnt und ändert nichts. Das Ändern einer Einstellung
schreibt den Speicher in kanonischer Form neu und erfordert
Schreibzugriff auf `/System/Settings` — ein gewöhnliches Konto kann die
Einstellungen lesen, aber nicht ändern.

- `os.loginType` — `text` oder `graphical`: welchen Sitzungstyp der
  Anmeldedienst für einen authentifizierten Benutzer startet.
  `graphical` (die Vorgabe) startet nach der Authentifizierung direkt
  die Desktop-Sitzung und fällt auf die Textanmeldung zurück, wenn die
  Maschine keine ausführen kann; `text` startet die Shell des Kontos —
  der Desktop lässt sich weiterhin bei Bedarf mit dem Befehl `desktop`
  starten.
- `cache.all` — `on` oder `off`: der Haupt-Caching-Schalter. `on` (die
  Vorgabe) lässt jede Cache-Klasse unten ihrer eigenen Einstellung
  folgen; `off` ist eine Obergrenze, die jeden Speicher-Cache
  unabhängig von den Einstellungen je Klasse deaktiviert.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` oder `off`: die Schalter je Klasse für die
  vier rückgewinnbaren Speicher-Caches (Dateisystem-, Vollplatten-
  Block-, entpackte-Cluster- und Anwendungsstart-Cache). `auto` (die
  Vorgabe) lässt den Speicherdruck-Manager die Klasse steuern; `off`
  deaktiviert sie ganz. Es gibt kein `on` je Klasse: eine Klasse kann
  nicht gezwungen werden, Speicherdruck zu ignorieren. Eine Klasse ist
  praktisch `off`, sobald `cache.all` auf `off` steht.

Jeder Cache ist ein rückgewinnbarer Beschleuniger, niemals die Quelle
der Wahrheit; das Abschalten eines oder aller macht die betroffene
Arbeit daher nur langsamer — es ändert niemals ein Ergebnis.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` oder `false`: die
  stackweiten Schalter der Adressfamilien. Beide sind standardmäßig
  `true`. Eine deaktivierte Familie bindet keine Adressen, beantwortet
  keine Pakete und lehnt einen Socket dieser Familie mit einem
  typisierten Fehler ab — nie ein stilles Verwerfen.
- `net.ipv6.privacy` — `true` oder `false`: ob der Stack temporäre
  (Privacy-)IPv6-Adressen zusätzlich zur stabilen Adresse bildet.
  `false` (die Vorgabe) nutzt nur die stabile SLAAC-Adresse.
- `net.tcp.syncookies` — `auto` oder `always`: die Abwehr gegen
  SYN-Fluten. `auto` (die Vorgabe) hält eine begrenzte Halb-offen-
  Warteschlange und weicht bei Überlauf auf zustandslose Cookies aus;
  `always` beantwortet jede Verbindungsanfrage zustandslos. Es gibt
  kein `off` — eine ungeschützte Verbindungswarteschlange ist keine
  Einstellung.
- `net.tcp.keepalive` — `true` oder `false`: ob TCP-Verbindungen auf
  einer inaktiven Leitung Keepalive-Prüfungen senden. `false` (die
  Vorgabe) prüft nie und trennt eine inaktive Verbindung nie; `true`
  prüft einen inaktiven Gegenpart nach dem üblichen Intervall und
  trennt die Verbindung, wenn er nicht mehr antwortet.
- `net.tcp.ecn` — `true` oder `false`: ob TCP-Verbindungen Explicit
  Congestion Notification aushandeln. `false` (die Vorgabe) lässt
  Verbindungen Not-ECT; `true` bietet ECN im Handshake an und behandelt
  danach eine Überlastmarkierung als Signal zum Drosseln, statt einen
  Paketverlust zu erzwingen.
- `time.servers` — `none` oder eine kommagetrennte Liste von
  Netzwerkzeitservern, jeder ein Hostname oder eine Adresse. `none` (die
  Vorgabe) bedeutet, dass die Uhr nie aus dem Netz gestellt wird: TAIRiX
  hat keinen eigenen Zeitserver-Pool, also ist das Benennen eines Servers
  die Entscheidung des Betreibers.
- `time.refresh` — `6h`, `12h`, `1d`, `2d` oder `7d`: wie viel Laufzeit
  zwischen erneuten Uhrabfragen vergeht, sobald die Zeit bekannt ist. `1d`
  ist die Vorgabe. Eine ungestellte, unplausible oder lange veraltete Uhr
  wird unabhängig davon sofort korrigiert, sobald das Netz es erlaubt.
- `input.mouse.debounce` — ganze Millisekunden, standardmäßig `25`, `0`
  deaktiviert, höchstens `100`: wie lange nach dem Loslassen einer Maustaste
  der nächste Druck derselben Taste als Kontaktprellen ignoriert wird, statt
  als neuer Klick zu gelten. Ein abgenutzter Schalter kann wenige
  Millisekunden nach dem Loslassen einen zweiten Druck melden, der als ein
  Klick gemeint war. Bei einer Maus, deren Schnellfeuermodus absichtlich
  Klickpaare sendet, `0` setzen.

Die `net.*`-Einstellungen liest der Netzwerk-Stack; eine Änderung wirkt,
sobald der Stack seine Konfiguration das nächste Mal anwendet.

## OPTIONS

- `-h, -?` — die Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `configure` — jede Einstellung auflisten.
- `configure os.loginType` — den vorgegebenen Sitzungstyp anzeigen.
- `configure os.loginType graphical` — in die grafische Anmeldung
  starten.
- `configure cache.all off` — jeden Speicher-Cache systemweit
  deaktivieren.
- `configure cache.filesystem off` — nur den Dateisystem-Cache
  deaktivieren.

## EXIT STATUS

- `0` — Auflistung, Wert, Kurzhilfe oder Änderung wurden abgeschlossen.
- `1` — der Speicher konnte nicht gelesen oder geschrieben werden
  (z. B. darf der Aufrufer Systemeinstellungen nicht ändern) oder die
  Ausgabe konnte nicht zugestellt werden.
- `2` — die Befehlszeile wurde nicht verstanden, der Schlüssel ist
  unbekannt oder der Wert liegt außerhalb der Menge des Schlüssels.

## ENVIRONMENT

- `LANG` — die bevorzugte Sprache der Kurzhilfe (ein BCP-47-Kennzeichen
  wie `fr-FR`).

## SEE ALSO

- `man`
