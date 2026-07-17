## NAME

false — gwneud dim, yn aflwyddiannus

## SYNOPSIS

`false [ignored arguments]`

## DESCRIPTION

Mae'n gorffen gyda'r statws `1`, gan anwybyddu pob ymresymiad. Mae
sgriptiau'n ei ddefnyddio lle bynnag y bo angen gorchymyn sy'n methu
bob tro — fel amod sydd wastad yn anwir neu fethiant bwriadol.

Dim ond ymresymiad **cyntaf** o `-h`, `-?` neu `--help` a anrhydeddir
(y safle y mae `false` GNU yn anrhydeddu `--help` ynddo); mewn unrhyw
safle diweddarach anwybyddir y tocynnau hynny fel popeth arall. Yn
wahanol i `false --help` GNU, sy'n dal i orffen gyda `1`, mae cymorth
byr a gyflwynwyd yn gorffen gyda `0` yma — confensiwn cymorth byr
TAIRiX.

## OPTIONS

- `-h, -?` — (ymresymiad cyntaf yn unig) dangos cymorth byr y gorchymyn
  hwn ei hun.

## EXAMPLES

- `false` — methu.
- `until false; do …; done` — rhedeg y corff unwaith (mae'r amod wastad
  yn anwir).

## EXIT STATUS

- `1` — bob amser (holl bwrpas yr offeryn).
- `0` — cyflwynwyd cymorth byr y gofynnwyd amdano.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `true`
- `man`
