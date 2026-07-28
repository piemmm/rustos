## NAME

host — datrys enw drwy DNS

## SYNOPSIS

`host [-t type] name`

## DESCRIPTION

Yn datrys enw parth i'w gyfeiriadau gan ddefnyddio datryswr sylfaenol y system
ac yn argraffu pob ateb, un fesul llinell. Heb `-t`, ymholir cofnodion `A`
(IPv4) a `AAAA` (IPv6) fel ei gilydd; mae `-t type` yn cyfyngu'r chwiliad i
un.

Darllenir y gweinyddion DNS ailadroddol i'w hymholi o gyfluniad y gwesteiwr
drwy'r API Gwybodaeth System — yr un set weithredol ag a adroddir gan y
darlleniad `state:net/resolver/servers` — a dilysir pob ateb cyn dangos
cyfeiriad. Nid oes `/etc/resolv.conf` na ffeil gwesteiwyr lleol.

Dim ond y cofnodion cyfeiriad `A` a `AAAA` a gefnogir; gwrthodir mathau eraill
(`MX`, `TXT`, ac ati) yn hytrach na'u trin yn dawel fel `A`. Mae enw nad yw'n
bodoli yn argraffu `Host <name> not found: 3(NXDOMAIN)`; pan na ellir cyrraedd
unrhyw weinydd, mae `host` yn adrodd am oediad ar yr allbwn gwall.

## OPTIONS

- `-t, --type` — y math o gofnod DNS i'w ymholi: `A` neu `AAAA` (heb hidlo
  prif lythrennau). Hebddi, ymholir y ddau.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `host example.com` — cyfeiriadau IPv4 ac IPv6 yr enw.
- `host -t AAAA example.com` — dim ond y cyfeiriadau IPv6.

## EXIT STATUS

- `0` — canfuwyd o leiaf un cyfeiriad (neu ysgrifennwyd y cymorth byr).
- `1` — ni ddatrysodd yr enw i unrhyw gyfeiriad (ateb negyddol, oediad neu
  fethiant y datryswr).
- `2` — ni ddeallwyd y llinell orchymyn, neu ni ellid ysgrifennu'r allbwn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
