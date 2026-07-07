## NAME

true — gwneud dim, yn llwyddiannus

## SYNOPSIS

`true [ignored arguments]`

## DESCRIPTION

Mae'n gorffen gyda'r statws `0`, gan anwybyddu pob ymresymiad. Mae
sgriptiau'n ei ddefnyddio lle bynnag y bo angen gorchymyn sy'n llwyddo
bob tro — fel gorchymyn dal lle, amod sydd wastad yn wir, neu gorff
dolen.

Dim ond ymresymiad **cyntaf** o `-h`, `-?` neu `--help` a anrhydeddir
(y safle y mae `true` GNU yn anrhydeddu `--help` ynddo); mewn unrhyw
safle diweddarach anwybyddir y tocynnau hynny fel popeth arall.

## OPTIONS

- `-h, -?` — (ymresymiad cyntaf yn unig) dangos cymorth byr y gorchymyn
  hwn ei hun.

## EXAMPLES

- `true` — llwyddo.
- `while true; do …; done` — dolennu nes ei dorri.

## EXIT STATUS

- `0` — bob amser (holl bwrpas yr offeryn).
- `1` — ni ellid ysgrifennu cymorth byr y gofynnwyd amdano.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `false`
- `man`
