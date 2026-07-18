## NAME

sleep — oedi am swm o gyfnodau amser

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

Yn oedi am swm y cyfnodau a roddir ac yna'n gorffen.

Mae pob `NUMBER` yn werth pwynt arnawf; mae `SUFFIX` un llythyren yn ei
raddio: `s` am eiliadau (y rhagosodiad), `m` am funudau, `h` am oriau, a
`d` am ddyddiau. Adiwyd sawl operand at ei gilydd, felly mae
`sleep 1m 30s` yn oedi am naw deg eiliad. Mae `inf` (neu `infinity`) yn
oedi nes i'r broses gael ei lladd.

Yn wahanol i amseru cragen ei hun, mae `sleep` yn cysgu oddi ar y
prosesydd: caiff y dasg ei pharcio nes bod y cyfnod wedi mynd heibio, ac ni
fydd byth yn troi craidd yn wag.

Mae gwerth negatif, `nan`, ôl-ddodiad anhysbys, neu nodau ychwanegol ar ôl
y rhif yn `invalid time interval`. Mae peidio â rhoi unrhyw operand yn
`missing operand`.

Nid yw'r gorchymyn hwn yn argraffu fersiwn system; nid oes gan TAIRiX
linyn o'r fath, felly — yn wahanol i GNU `sleep` — nid oes ganddo'r opsiwn
`--version`.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn.
- `--` — gorffen dosrannu opsiynau; mae unrhyw arg diweddarach yn operand.

## EXAMPLES

- `sleep 5` — oedi am bum eiliad.
- `sleep 1.5h` — oedi am naw deg munud.
- `sleep 1m 30s` — oedi am naw deg eiliad (adiwyd yr operandau).
- `sleep inf` — oedi nes i'r broses gael ei lladd.

## EXIT STATUS

- `0` — aeth y cyfnod heibio, neu ysgrifennwyd cymorth byr a ofynnwyd
  amdano.
- `1` — methodd ysgrifennu'r cymorth byr.
- `2` — ni ddeallwyd y llinell orchymyn (opsiwn anhysbys, operand ar goll,
  neu gyfnod amser annilys).

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `top`
