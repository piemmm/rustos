## NAME

seq — argraffu dilyniant o rifau

## SYNOPSIS

`seq [-f format] [-s string] [-w] [first [increment]] last`

## DESCRIPTION

Mae'n argraffu'r rhifau o `first` i `last`, mewn camau o `increment`,
un fesul llinell yn ragosodedig. Rhagosodiad `first` neu `increment` a
hepgorwyd yw 1 — gan gynnwys pan fo `last` yn llai na `first`, felly
nid yw `seq 5 1` yn argraffu dim. Daw'r dilyniant i ben pan fyddai
ychwanegu `increment` yn mynd heibio `last`.

Darllenir y tri operand fel gwerthoedd pwynt arnawf; mae `increment`
fel arfer yn bositif pan fo `first` islaw `last` ac yn negatif pan fo
uwchlaw, ac ni chaiff fod yn sero. Gall `last` fod yn `inf` i gyfrif
am byth. Mae trachywiredd rhagosodedig yr allbwn yn dilyn sillafiad yr
operandau (mae `seq 1 0.25 2` yn argraffu dau le degol), a chynhyrchir
rhediadau cyfanrifau plaen yn union, waeth pa mor fawr yw'r rhifau.

Mae sganio opsiynau'n dod i ben wrth yr operand cyntaf, ac operand yw
rhif negatif blaen, nid opsiwn: mae `seq -5 5` yn cyfrif o -5.

## OPTIONS

- `-f, --format <format>` — argraffu pob rhif trwy'r `<format>` pwynt
  arnawf arddull-printf (un cyfarwyddeb `%` o fath `e`, `f`, `g` neu
  `a`, priflythrennau neu lythrennau bach, gyda'r baneri, lled a
  thrachywiredd arferol). Ni ellir ei gyfuno ag `-w`.
- `-s, --separator <string>` — gwahanu rhifau â `<string>` yn lle
  llinell newydd. Mae'r allbwn yn dal i orffen â llinell newydd.
- `-w, --equal-width` — padio pob rhif â seroau blaen hyd led
  cyffredin. Ni ellir ei gyfuno ag `-f`.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.
- `--` — gorffen dosrannu opsiynau; mae pob ymresymiad diweddarach yn
  operand.

## EXAMPLES

- `seq 5` — argraffu 1 hyd 5.
- `seq 2 5` — argraffu 2 hyd 5.
- `seq 1 2 10` — argraffu'r odrifau 1 hyd 9.
- `seq 5 -1 1` — cyfrif i lawr o 5 i 1.
- `seq -w 8 10` — argraffu `08`, `09`, `10`.
- `seq -s , 3` — argraffu `1,2,3`.
- `seq -f %.2f 3` — argraffu `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — ysgrifennwyd y dilyniant (neu gymorth byr y gofynnwyd amdano).
- `1` — peidiodd yr allbwn â derbyn beitiau.
- `2` — ni ddeallwyd y llinell orchymyn (opsiwn anhysbys, rhif
  annilys, cynyddiad sero neu fformat gwael).

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `yes`
- `man`
