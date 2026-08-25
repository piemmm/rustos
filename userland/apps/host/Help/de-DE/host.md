## NAME

host — einen Namen über DNS auflösen

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

Löst einen Domänennamen mit dem Stub-Resolver des Systems in seine Adressen
auf und gibt jede Antwort in einer eigenen Zeile aus. Ohne `-t` werden sowohl
die `A`- (IPv4) als auch die `AAAA`-Einträge (IPv6) abgefragt; `-t type`
beschränkt die Abfrage auf einen.

Die abzufragenden rekursiven DNS-Server werden aus der Host-Konfiguration über
die System-Informations-API gelesen — dieselbe aktive Menge, die der Abruf
`state:net/resolver/servers` meldet — und jede Antwort wird geprüft, bevor
eine Adresse angezeigt wird. Es gibt kein `/etc/resolv.conf` und keine lokale
Hosts-Datei.

Ein Operand, der eine IPv4- oder IPv6-Adresse ist, bedeutet eine
**Rückwärts**-Abfrage: er wird auf den `in-addr.arpa`- bzw.
`ip6.arpa`-Namen der Adresse umgeschrieben, der Standardtyp wird `PTR`, und
ein gefundener Eintrag erscheint als
`<reverse-name> domain name pointer <name>.`

Nur die Einträge `A`, `AAAA` und `PTR` werden unterstützt; andere Typen (`MX`,
`TXT` und so weiter) werden abgelehnt, statt stillschweigend als `A` behandelt
zu werden. Ein nicht vorhandener Name gibt `Host <name> not found:
3(NXDOMAIN)` aus; ist kein Server erreichbar, meldet `host` eine
Zeitüberschreitung auf der Standardfehlerausgabe.

## OPTIONS

- `-t, --type` — der abzufragende DNS-Eintragstyp: `A`, `AAAA` oder `PTR`
  (Groß-/Kleinschreibung wird ignoriert). Ohne diese Option fragt ein Name
  `A` und `AAAA` ab, eine Adresse `PTR`.
- `-?, --help` — die eigene Kurzhilfe dieses Befehls zeigen.

## EXAMPLES

- `host example.com` — die IPv4- und IPv6-Adressen des Namens.
- `host -t AAAA example.com` — nur die IPv6-Adressen.
- `host 10.0.2.2` — der Name, auf den diese Adresse zurückverweist.

## EXIT STATUS

- `0` — mindestens eine Adresse wurde gefunden (oder die Kurzhilfe wurde
  geschrieben).
- `1` — der Name löste keine Adresse auf (negative Antwort,
  Zeitüberschreitung oder Resolver-Fehler).
- `2` — die Befehlszeile wurde nicht verstanden, oder die Ausgabe konnte
  nicht geschrieben werden.

## ENVIRONMENT

- `LANG` — die bevorzugte Locale für die Kurzhilfe (ein BCP-47-Etikett wie
  `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
