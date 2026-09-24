## NAME

dns-sd — pori, datrys a chwilio am wasanaethau'r cyswllt lleol

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Yn holi'r rhwydwaith lleol — y cyswllt — am y gwasanaethau y mae'n eu cynnig,
drwy wasanaeth darganfod cyswllt lleol y system. Nid yw `dns-sd` byth yn
siarad DNS amlddarlledu ei hun: mae'r gwasanaeth darganfod yn holi'r segment
ar ei ran ac yn derbyn pob cais yn ôl awdurdod y rhaglen ei hun.

Gyda `-B` mae'n pori enghreifftiau un math o wasanaeth, megis `_ipp._tcp` ar
gyfer argraffwyr, ac yn argraffu pob enghraifft wrth iddi gael ei hychwanegu
neu ei thynnu. Heb fath, mae'n rhestru pob math o wasanaeth y mae'r cyswllt
yn ei gynnig. Gyda `-L` mae'n datrys un enghraifft i'r gwesteiwr a'r porth lle
ceir hi ac yn argraffu'r hyn y mae'r enghraifft yn ei ddweud amdani ei hun (ei
phriodoleddau `TXT`). Gyda `-G` mae'n chwilio am gyfeiriadau gwesteiwr o dan
`local`.

Mae pob ateb yn enwi'r rhyngwyneb y dysgwyd ef arno, ac mae llinell `Flush`
yn golygu nad oes dim a ddysgwyd ar y rhyngwyneb hwnnw yn hysbys mwyach —
aeth ei gyswllt i lawr, neu ailddechreuodd y gwasanaeth. Dewiswyd pob enw a
argraffir gan beiriant arall ar y cyswllt, felly fe'i dangosir wedi'i ddianc,
ar ffurf cyflwyno DNS: bwlch fel `\032`, nod rheoli fel ei god degol.

Mae pori pob math, neu fath na roddwyd i'r cyfrif sy'n rhedeg, yn gofyn am
`CAP_NET_DISCOVER_ALL`, sydd gan gyfrif gweinyddwr yn unig. `local` yw'r unig
barth ar y cyswllt.

## OPTIONS

- `-B` — pori enghreifftiau math o wasanaeth, neu bob math pan na roddir un.
- `-L` — datrys un enghraifft o fath o wasanaeth.
- `-G` — chwilio am gyfeiriadau IPv4 (`v4`), IPv6 (`v6`) neu'r ddau (`v4v6`)
  gwesteiwr.
- `-t` — stopio ar ôl hyn o eiliadau yn lle rhedeg nes cael ei dorri ar
  draws.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — yr argraffwyr ar y cyswllt, wrth iddynt fynd a dod.
- `dns-sd -t 5 -B` — pob math o wasanaeth a welwyd o fewn pum eiliad.
- `dns-sd -L "Hall Printer" _ipp._tcp` — lle ceir un argraffydd.
- `dns-sd -G v4v6 printer.local` — cyfeiriadau gwesteiwr.

## EXIT STATUS

- `0` — cwblhaodd y gorchymyn ei rediad (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthododd y gwasanaeth darganfod y cais neu nid yw'n rhedeg.
- `2` — ni ddeallwyd y llinell orchymyn, neu nid oedd modd ysgrifennu'r
  allbwn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 megis
  `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
