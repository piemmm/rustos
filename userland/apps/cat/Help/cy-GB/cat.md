## NAME

cat — cydgadwynu ffeiliau i'r allbwn safonol

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Mae'n darllen pob operand ffeil yn ei drefn ac yn ysgrifennu ei feitiau
i'r allbwn safonol. Mae'r operand `-` yn enwi'r mewnbwn safonol, a heb
operand y mewnbwn safonol yw'r unig ffynhonnell.

Gall operand hefyd fod yn gyfeiriad adnodd teipiedig fel `sys:random`:
caiff ei agor drwy ddatryswr adnoddau'r system (gyda gwiriad galluoedd)
yn hytrach na'r system ffeiliau — mae `cat sys:random` yn ffrydio
beitiau ar hap. Mae cyfeiriad `info:`, `state:` neu `stats:` yn enwi
gwerth system teipiedig yn hytrach na ffrwd; caiff ei ddarllen drwy
wasanaeth gwybodaeth y system, felly mae `cat info:mem/physical` yn
argraffu'r gwerth hwnnw, a gwrthodir darlleniad nad yw'r cyfrif â hawl
iddo gan enwi'r gallu sydd ei angen. Mae cyfeiriad camffurfiedig mewn
gofod enwau cofrestredig yn wall, byth yn enw ffeil.

Gydag `-n` rhifir llinellau'r allbwn yn ddi-dor ar draws pob
ffynhonnell, felly rhifir llinell sy'n pontio dwy ffynhonnell unwaith
yn union, pan ymddengys ei beit cyntaf. Mae `-b` yn rhifo'r llinellau
nad ydynt yn wag yn unig ac yn drech nag `-n`. Mae `-s` yn atal
llinellau gwag cyfagos a ailadroddir, ac ni chaiff llinell a ataliwyd
ei hysgrifennu na'i rhifo.

Mae'r opsiynau marcio'n gwneud beitiau anweledig yn weladwy: mae `-E`
yn argraffu `$` cyn pob llinell newydd, mae `-T` yn argraffu TAB fel
`^I`, ac mae `-v` yn argraffu beitiau rheoli eraill fel `^X` a beitiau
nad ydynt yn ASCII yn nodiant `M-`. `-e`, `-t` ac `-A` yw'r
cyfuniadau arferol `-vE`, `-vT` a `-vET`.

Mae ffynhonnell na ellir ei darllen yn atal y gorchymyn cyn cyffwrdd ag
unrhyw ffynhonnell ddiweddarach; erys y beitiau a ysgrifennwyd eisoes.

## OPTIONS

- `-A, --show-all` — cyfwerth â `-vET`.
- `-b, --number-nonblank` — rhifo llinellau allbwn nad ydynt yn wag;
  yn drech nag `-n`.
- `-e` — cyfwerth â `-vE`.
- `-E, --show-ends` — argraffu `$` ar ddiwedd pob llinell.
- `-n, --number` — rhifo llinellau'r allbwn, yn ddi-dor ar draws pob
  ffynhonnell.
- `-s, --squeeze-blank` — atal llinellau gwag cyfagos a ailadroddir.
- `-t` — cyfwerth â `-vT`.
- `-T, --show-tabs` — argraffu nodau TAB fel `^I`.
- `-u` — fe'i derbynnir a'i anwybyddu; mae'r allbwn eisoes heb ei
  fyffro.
- `-v, --show-nonprinting` — defnyddio nodiant `^` ac `M-` ar gyfer
  beitiau rheoli a rhai nad ydynt yn ASCII, ac eithrio llinell newydd
  a TAB.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `cat notes.txt` — ysgrifennu `notes.txt` i'r allbwn safonol.
- `cat a.txt - b.txt` — ysgrifennu `a.txt`, yna'r mewnbwn safonol, yna
  `b.txt`.
- `cat -n log.txt` — rhifo pob llinell allbwn.
- `cat -bs draft.txt` — rhifo'r llinellau nad ydynt yn wag a gwasgu'r
  rhediadau gwag.
- `cat -A config.txt` — gwneud terfynau llinell, tabiau a beitiau
  rheoli'n weladwy.
- `cat -- -n` — ysgrifennu'r ffeil o'r enw `-n`.

## EXIT STATUS

- `0` — ysgrifennwyd pob ffynhonnell.
- `1` — ni ellid darllen ffynhonnell, neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `ls`
- `man`
