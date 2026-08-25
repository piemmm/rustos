## NAME

ping — anfon ceisiadau atsain ICMP at westeiwr rhwydwaith

## SYNOPSIS

`ping [option...] gwesteiwr`

## DESCRIPTION

Anfona geisiadau atsain ICMP (IPv4) neu ICMPv6 (IPv6) at westeiwr ac yn
dangos pob ateb gyda'i amser mynd a dod, ac yna grynodeb terfynol.

Mae'r ceisiadau'n llifo trwy soced atsain ICMP a agorwyd o'r pentwr
rhwydwaith yn y gofod defnyddiwr, wedi'i ddiogelu gan `CAP_NET` a
`CAP_NET_RAW` ac wedi'i archwilio. Mae'r pentwr yn berchen ar y dynodwr
atsain, felly dim ond atebion i'w geisiadau ei hun y mae soced yn eu
derbyn.

Cyfeiriad IPv4 neu IPv6 llythrennol neu enw gwesteiwr yw'r nod. Caiff enw
ei ddatrys gan ddatryswr y system, gan ddefnyddio'r gweinyddion ailadroddus
sydd wedi'u ffurfweddu ar y peiriant; nid oes angen ymholiad ar gyfer
cyfeiriad llythrennol, felly mae'n gweithio hyd yn oed heb ddatryswr wedi'i
ffurfweddu. Mae enw nad yw'n datrys i unrhyw gyfeiriad o'r teulu a ofynnwyd
amdano yn dod â'r rhediad i ben gan nodi'r rheswm.

Yn ddiofyn mae pob cais yn cario data ar hap ag entropi uchel, wedi'i dynnu
o'r newydd ar gyfer pob cais. Mae hyn yn fwriadol: byddai cyswllt sy'n
cywasgu neu'n dad-ddyblygu traffig fel arall yn adrodd trwybwn a hwyrni nad
ydynt yn dweud dim am ei allu gwirioneddol. Cymharir y beitiau a ddaw'n ôl
â'r rhai a anfonwyd, felly mae llwyth ar hap hefyd yn wiriad cyfanrwydd fesul
pecyn. Defnyddiwch `-p` ar gyfer patrwm sefydlog pan mai llwyth
determinyddol sydd eisiau.

Yn ddiofyn mae `ping` yn anfon un cais y eiliad nes ei atal; mae `-c` yn
cyfyngu'r nifer. Mae pob ateb yn nodi'r ffynhonnell, y rhif dilyniant a'r
amser; mae cais heb ateb o fewn y terfyn amser yn argraffu llinell dod i
ben. Mae'r crynodeb yn nodi'r pecynnau a drosglwyddwyd ac a dderbyniwyd,
y ganran colled, a'r amseroedd mynd a dod lleiaf, cyfartalog a mwyaf. Mae
`-q` yn dangos y pennyn a'r crynodeb yn unig.

Nid yw'r amser byw IP yn cael ei ddatgelu gan ryngwyneb y soced atsain;
yn wahanol i rai gweithrediadau `ping`, nid yw llinell ateb felly'n cario
maes `ttl=`.

## OPTIONS

- `-c, --count` — stopio ar ôl anfon y nifer hwn o geisiadau.
- `-i, --interval` — eiliadau rhwng ceisiadau (degol, e.e. `0.5`).
- `-s, --size` — maint y llwyth mewn beit.
- `-p, --pattern` — cynnwys y llwyth: `random` (rhagosodiad, entropi
  uchel) neu linyn o ddigidau hecsadegol o hyd eilrif fel patrwm beit sy'n
  ailadrodd, e.e. `-p ff00`.
- `-W, --timeout` — eiliadau i aros am bob ateb.
- `-w, --deadline` — terfyn amser cyffredinol y rhediad, mewn eiliadau.
- `-4, --ipv4` — mynnu nod IPv4.
- `-6, --ipv6` — mynnu nod IPv6.
- `-n, --numeric` — allbwn rhifol. Derbynnir ac nid oes iddo effaith: ni
  chyflawnir datrysiad gwrthdro erioed, felly mae cyfeiriadau'r atebion yn
  rhifol yn barod.
- `-q, --quiet` — tawel: y pennyn a'r crynodeb terfynol yn unig.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `ping 10.0.2.2` — pingio gwesteiwr IPv4 nes ei atal.
- `ping -c 4 fe80::1` — anfon pedwar cais at westeiwr IPv6.
- `ping -c 10 -i 0.2 10.0.0.1` — deg cais, un bob 200 ms.
- `ping -q -c 100 10.0.0.1` — rhediad tawel, crynodeb yn unig.

## EXIT STATUS

- `0` — derbyniwyd o leiaf un ateb (neu ysgrifennwyd y cymorth byr).
- `1` — ni chafodd unrhyw gais ateb.
- `2` — ni ddeallwyd y llinell orchymyn, ni ddatryswyd y nod, neu ni ellid
  agor y soced.

## ENVIRONMENT

- `LANG` — yr ardal ffafredig ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `host`
- `ss`
- `sysinfo`
- `man`
