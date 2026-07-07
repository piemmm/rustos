## NAME

yes — allbynnu llinell o destun dro ar ôl tro

## SYNOPSIS

`yes [string...]`

## DESCRIPTION

Mae'n ysgrifennu ei operandau, wedi'u huno â bylchau sengl — neu `y`
pan na roddir yr un — ac yna nod llinell newydd, dro ar ôl tro, nes i'r
allbwn beidio â derbyn beitiau (pibell wedi cau) neu i'r broses gael ei
therfynu. Ei waith hanesyddol yw bwydo ateb cadarnhaol i orchymyn sy'n
holi; ei waith modern yw bod yn ffynhonnell rad o destun a ailadroddir.

Mae sganio opsiynau'n dod i ben wrth yr operand cyntaf, felly mae
`yes a -x` yn argraffu `a -x`. Mae opsiwn anhysbys cyn yr operandau yn
wall; ysgrifennwch `yes -- -x` i argraffu llinyn sy'n edrych fel
opsiwn.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.
- `--` — gorffen dosrannu opsiynau; mae pob ymresymiad diweddarach yn
  operand.

## EXAMPLES

- `yes` — argraffu `y` nes ei dorri.
- `yes hello world` — argraffu `hello world` nes ei dorri.
- `yes -- -x` — argraffu `-x` (ar ôl `--`, gall operandau edrych fel
  opsiynau).

## EXIT STATUS

- `0` — cyflwynwyd cymorth byr y gofynnwyd amdano.
- `1` — peidiodd yr allbwn â derbyn beitiau (unig amod stopio'r
  offeryn).
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `true`
- `man`
