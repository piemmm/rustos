## NAME

dns-sd — Dienste der lokalen Verbindung durchsuchen, auflösen und nachschlagen

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Fragt das lokale Netz — die Verbindung — nach den Diensten, die es anbietet,
über den Erkennungsdienst des Systems für die lokale Verbindung. `dns-sd`
spricht selbst nie Multicast-DNS: Der Erkennungsdienst fragt das Segment in
seinem Auftrag und lässt jede Anfrage nach der eigenen Befugnis des Programms
zu.

Mit `-B` durchsucht es die Instanzen eines Diensttyps, etwa `_ipp._tcp` für
Drucker, und gibt jede Instanz aus, wenn sie hinzukommt oder verschwindet.
Ohne Typ listet es jeden Diensttyp auf, den die Verbindung anbietet. Mit `-L`
löst es eine Instanz zu dem Host und Port auf, unter dem sie erreichbar ist,
und gibt aus, was die Instanz über sich selbst sagt (ihre `TXT`-Attribute).
Mit `-G` schlägt es die Adressen eines Hosts unter `local` nach.

Jede Antwort nennt die Schnittstelle, auf der sie erfahren wurde, und eine
`Flush`-Zeile bedeutet, dass alles auf dieser Schnittstelle Erfahrene nicht
mehr bekannt ist — ihre Verbindung ist ausgefallen, oder der Dienst hat neu
begonnen. Jeder ausgegebene Name wurde von einer anderen Maschine auf der
Verbindung gewählt und wird daher maskiert in DNS-Darstellungsform gezeigt:
ein Leerzeichen als `\032`, ein Steuerzeichen als sein Dezimalcode.

Das Durchsuchen aller Typen oder eines Typs, der dem laufenden Konto nicht
gewährt wurde, erfordert `CAP_NET_DISCOVER_ALL`, das nur ein
Administratorkonto trägt. Die einzige Domäne auf der Verbindung ist `local`.

## OPTIONS

- `-B` — die Instanzen eines Diensttyps durchsuchen, oder jeden Typ, wenn
  keiner angegeben ist.
- `-L` — eine Instanz eines Diensttyps auflösen.
- `-G` — die IPv4- (`v4`), IPv6- (`v6`) oder beide (`v4v6`) Adressen eines
  Hosts nachschlagen.
- `-t` — nach so vielen Sekunden anhalten, statt bis zur Unterbrechung zu
  laufen.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — die Drucker auf der Verbindung, wie sie kommen und
  gehen.
- `dns-sd -t 5 -B` — jeder innerhalb von fünf Sekunden gesehene Diensttyp.
- `dns-sd -L "Hall Printer" _ipp._tcp` — wo ein Drucker erreichbar ist.
- `dns-sd -G v4v6 printer.local` — die Adressen eines Hosts.

## EXIT STATUS

- `0` — der Befehl ist vollständig gelaufen (oder die Kurzhilfe wurde
  geschrieben).
- `1` — der Erkennungsdienst hat die Anfrage abgelehnt oder läuft nicht.
- `2` — die Befehlszeile wurde nicht verstanden, oder die Ausgabe konnte nicht
  geschrieben werden.

## ENVIRONMENT

- `LANG` — das bevorzugte Gebietsschema für die Kurzhilfe (ein BCP-47-Tag wie
  `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
