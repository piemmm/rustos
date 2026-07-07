## NAME

basename — tynnu'r cyfeiriadur a'r ôl-ddodiad oddi ar enwau

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Mae'n argraffu cydran olaf pob sillafiad llwybr: tynnir slaesau
terfynol, yna popeth hyd at y slaes olaf sy'n weddill, gan ei chynnwys.
Mae'r llawdriniaeth yn gwbl eirfaol — ni chaiff unrhyw lwybr ei ddatrys
na'i gyffwrdd ar ddisg. Gydag `suffix` (yr ail operand, neu `-s`),
tynnir `suffix` terfynol hefyd, oni bai mai dyna'r enw cyfan sy'n
weddill.

Ni ddatgymalir gwreiddyn byth: `basename /` yw `/`, ac — union gyfateb
fforest storio RustOS — `basename Home:/` yw `Home:/`. Mae gwreiddyn
alias (`Home:/`, `System:/`, …) yn chwarae'n union y rôl y mae `/` yn
ei chwarae ar systemau POSIX.

Heb `-a` nac `-s`, derbynnir dau operand ar y mwyaf: yr enw ac
ôl-ddodiad dewisol. Gydag `-a` (neu `-s`, sy'n ei awgrymu), mae pob
operand yn enw.

## OPTIONS

- `-a, --multiple` — trin pob operand fel enw.
- `-s, --suffix <suffix>` — tynnu `suffix` terfynol o bob enw; mae'n
  awgrymu `-a`. Gellir ei sillafu hefyd yn `--suffix=<suffix>` neu
  wedi'i fwndelu (`-s.rs`).
- `-z, --zero` — gorffen pob canlyniad â NUL yn lle llinell newydd.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `basename /System/Apps/top.app` — argraffu `top.app`.
- `basename src/lib.rs .rs` — argraffu `lib`.
- `basename -s .rs -a a.rs b.rs` — argraffu `a` a `b`.
- `basename Home:/` — argraffu `Home:/` (ni ddatgymalir gwreiddyn
  byth).

## EXIT STATUS

- `0` — ysgrifennwyd y canlyniadau (neu'r cymorth byr).
- `1` — ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `dirname`
- `man`
