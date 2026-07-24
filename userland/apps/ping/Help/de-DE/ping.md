## NAME

ping — ICMP-Echo-Anfragen an einen Netzwerk-Host senden

## SYNOPSIS

`ping [option...] adresse`

## DESCRIPTION

Sendet ICMP- (IPv4) oder ICMPv6- (IPv6) Echo-Anfragen an einen Host und
zeigt jede Antwort mit ihrer Umlaufzeit an, gefolgt von einer
abschließenden Statistik.

Die Anfragen laufen über einen ICMP-Echo-Socket, der beim
Netzwerk-Stack im Benutzerbereich geöffnet wird, abgesichert durch
`CAP_NET` und `CAP_NET_RAW` und protokolliert. Der Stack besitzt die
Echo-Kennung, sodass ein Socket nur Antworten auf seine eigenen Anfragen
erhält. In dieser Version gibt es keine Namensauflösung; das Ziel muss
daher eine literale IPv4- oder IPv6-Adresse sein. Ein Hostname ist ein
Benutzungsfehler, kein stiller Fehlschlag.

Standardmäßig sendet `ping` eine Anfrage pro Sekunde bis zum Abbruch;
`-c` begrenzt die Anzahl. Jede Antwort nennt Quelle, Sequenznummer und
Zeit; eine Anfrage ohne Antwort innerhalb des Zeitlimits gibt eine
Zeitüberschreitungszeile aus. Die Abschlussstatistik nennt gesendete und
empfangene Pakete, den Verlustanteil sowie die minimale, mittlere und
maximale Umlaufzeit. `-q` zeigt nur den Kopf und die Statistik.

Die IP-Lebensdauer wird über die Echo-Socket-Schnittstelle nicht
offengelegt; anders als manche `ping`-Implementierungen trägt eine
Antwortzeile daher kein `ttl=`-Feld.

## OPTIONS

- `-c, --count` — nach dieser Anzahl von Anfragen anhalten.
- `-i, --interval` — Sekunden zwischen Anfragen (dezimal, z. B. `0.5`).
- `-s, --size` — Nutzlastgröße in Bytes.
- `-W, --timeout` — Sekunden Wartezeit je Antwort.
- `-w, --deadline` — Gesamtlaufzeit-Frist in Sekunden.
- `-4, --ipv4` — ein IPv4-Ziel verlangen.
- `-6, --ipv6` — ein IPv6-Ziel verlangen.
- `-n, --numeric` — numerische Ausgabe. Auf TAIRiX stets aktiv; zur
  Vertrautheit akzeptiert.
- `-q, --quiet` — still: nur Kopf und Abschlussstatistik.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls anzeigen.

## EXAMPLES

- `ping 10.0.2.2` — einen IPv4-Host bis zum Abbruch anpingen.
- `ping -c 4 fe80::1` — vier Anfragen an einen IPv6-Host senden.
- `ping -c 10 -i 0.2 10.0.0.1` — zehn Anfragen, alle 200 ms eine.
- `ping -q -c 100 10.0.0.1` — stiller Lauf, nur Statistik.

## EXIT STATUS

- `0` — mindestens eine Antwort empfangen (oder Kurzhilfe ausgegeben).
- `1` — keine Anfrage wurde beantwortet.
- `2` — Befehlszeile nicht verstanden oder Socket nicht zu öffnen.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Tag wie
  `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
