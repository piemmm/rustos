## NAME

dirname — tynnu'r gydran olaf oddi ar enwau

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Mae'n argraffu pob sillafiad llwybr â'i gydran olaf wedi'i thynnu:
tynnir slaesau terfynol, yna'r gydran olaf a'r slaesau o'i blaen. Mae'r
llawdriniaeth yn gwbl eirfaol — ni chaiff unrhyw lwybr ei ddatrys na'i
gyffwrdd ar ddisg. Rhiant sillafiad heb slaes ar ôl yw `.`; rhiant sy'n
gwacáu yw'r gwreiddyn.

Ni ddatgymalir gwreiddyn byth: `dirname /tools` yw `/`, ac — union
gyfateb fforest storio RustOS — `dirname Home:/tools` yw `Home:/`. Mae
gwreiddyn alias (`Home:/`, `System:/`, …) yn chwarae'n union y rôl y
mae `/` yn ei chwarae ar systemau POSIX.

## OPTIONS

- `-z, --zero` — gorffen pob canlyniad â NUL yn lle llinell newydd.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `dirname /System/Apps/top.app` — argraffu `/System/Apps`.
- `dirname src/lib.rs` — argraffu `src`.
- `dirname file` — argraffu `.` (dim rhan cyfeiriadur).
- `dirname Home:/tools` — argraffu `Home:/` (ni ddatgymalir gwreiddyn
  byth).

## EXIT STATUS

- `0` — ysgrifennwyd y canlyniadau (neu'r cymorth byr).
- `1` — ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `basename`
- `man`
