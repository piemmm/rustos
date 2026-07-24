## NAME

ping — anfon ceisiadau atsain ICMP at westeiwr rhwydwaith

## SYNOPSIS

`ping [option...] cyfeiriad`

## DESCRIPTION

Anfona geisiadau atsain ICMP (IPv4) neu ICMPv6 (IPv6) at westeiwr ac yn
dangos pob ateb gyda'i amser mynd a dod, ac yna grynodeb terfynol.

Mae'r ceisiadau'n llifo trwy soced atsain ICMP a agorwyd o'r pentwr
rhwydwaith yn y gofod defnyddiwr, wedi'i ddiogelu gan `CAP_NET` a
`CAP_NET_RAW` ac wedi'i archwilio. Mae'r pentwr yn berchen ar y dynodwr
atsain, felly dim ond atebion i'w geisiadau ei hun y mae soced yn eu
derbyn. Nid oes datrys enwau yn y fersiwn hwn, felly mae'n rhaid i'r nod
fod yn gyfeiriad IPv4 neu IPv6 llythrennol; mae enw gwesteiwr yn wall
defnydd, nid methiant tawel.

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
- `-W, --timeout` — eiliadau i aros am bob ateb.
- `-w, --deadline` — terfyn amser cyffredinol y rhediad, mewn eiliadau.
- `-4, --ipv4` — mynnu nod IPv4.
- `-6, --ipv6` — mynnu nod IPv6.
- `-n, --numeric` — allbwn rhifol. Bob amser mewn grym ar TAIRiX; derbynnir
  er cyfarwydd-dra.
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
- `2` — ni ddeallwyd y llinell orchymyn, neu ni ellid agor y soced.

## ENVIRONMENT

- `LANG` — yr ardal ffafredig ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
