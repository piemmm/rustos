## NAME

ss — offene Sockets auflisten

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Listet die offenen Sockets des Systems auf, eine Zeile je Socket: das
Transportprotokoll, den Verbindungszustand, die Füllstände der Empfangs-
und Sendewarteschlange, die lokale und die entfernte `address:port` und
— mit `-p` — den besitzenden Prozess.

Die Zeilen stammen aus der Socket-Liste der System-Informations-API, die
der Netzwerkstapel als privilegierte, auditierte Abfrage beantwortet: sie
nennt die Sockets jedes Prinzipals und den Gegenpunkt jeder Verbindung,
sodass das Auflisten aller Sockets `CAP_SYSINFO_GLOBAL` erfordert. Es
gibt kein `/proc/net`; einer Sitzung ohne diese Berechtigung wird das
mitgeteilt und `ss` beendet sich, statt eine leere Tabelle auszugeben.

Standardmäßig zeigt die Liste verbundene, nicht lauschende Sockets. `-l`
zeigt nur lauschende Sockets und `-a` beide; die Anzahl der verborgenen
Lauscher wird auf dem Standard-Informationsstrom (fd 3) vermerkt,
niemals in der Tabelle. `-t` und `-u` schränken das Protokoll ein und
`-4`/`-6` die Adressfamilie; ohne Angabe werden alle Protokolle und
Familien gezeigt. Ports und Adressen sind stets numerisch (TAIRiX hat
keine Dienstnamen-Datenbank), daher wird `-n` angenommen, ist aber immer
in Kraft. Eine nicht angegebene Adresse erscheint als `*` und ein
ungebundener Port als `*`; eine IPv6-Adresse wird in Klammern gesetzt,
damit der `:port`-Trenner eindeutig bleibt.

`ss` nimmt nur Optionen entgegen. Die Filterausdruck-Grammatik von
iproute2 (Zustands- und Adressfilter) ist nicht implementiert, daher ist
ein nackter Operand ein Nutzungsfehler statt eines stillschweigend
ignorierten Arguments.

## OPTIONS

- `-t, --tcp` — TCP-Sockets zeigen. Ohne `-t` und ohne `-u` werden
  beide Protokolle gezeigt.
- `-u, --udp` — UDP-Sockets zeigen.
- `-a, --all` — lauschende und verbundene Sockets zeigen.
- `-l, --listening` — nur lauschende Sockets zeigen.
- `-n, --numeric` — keine Dienstnamen auflösen. Auf TAIRiX immer in
  Kraft; aus Vertrautheit angenommen.
- `-p, --processes` — die Spalte des besitzenden Prozesses ergänzen
  (`pid=N`).
- `-4, --ipv4` — die Liste auf IPv4-Sockets beschränken.
- `-6, --ipv6` — die Liste auf IPv6-Sockets beschränken.
- `-H, --no-header` — die Kopfzeile unterdrücken.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls zeigen.

## EXAMPLES

- `ss` — die verbundenen, nicht lauschenden Sockets.
- `ss -a` — jeder Socket, lauschend und verbunden.
- `ss -l` — nur die lauschenden Sockets.
- `ss -tlp` — lauschende TCP-Sockets, mit dem besitzenden Prozess.
- `ss -u4` — die UDP-Sockets über IPv4.

## EXIT STATUS

- `0` — die Liste wurde erzeugt (oder die Kurzhilfe wurde geschrieben).
- `1` — die Socket-Abfrage wurde verweigert oder schlug fehl, oder die
  Ausgabe konnte nicht geschrieben werden.
- `2` — die Befehlszeile wurde nicht verstanden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Etikett
  wie `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
