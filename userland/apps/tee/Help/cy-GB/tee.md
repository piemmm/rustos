## NAME

tee — darllen o'r mewnbwn safonol ac ysgrifennu i'r allbwn safonol ac
i ffeiliau

## SYNOPSIS

`tee [option...] [file...]`

## DESCRIPTION

Mae'n copïo'r mewnbwn safonol i'r allbwn safonol ac i bob ffeil a
enwir, fel y gellir gweld a dal data piblinell ar unwaith. Crëir pob
ffeil os yw'n absennol a'i throsysgrifo oni bai fod `-a` yn atodi.
Adroddir am ffeil na ellir ei hagor na'i hysgrifennu ac mae'r rhediad
yn parhau gyda'r allbynnau sy'n weddill, yn ôl y modd `--output-error`
a ddewiswyd.

Nid oes `SIGPIPE` gan TAIRiX: mae defnyddiwr sy'n diflannu'n ymddangos
fel gwall ysgrifennu ar yr allbwn safonol — unig allbwn y gorchymyn
hwn a all fod yn bibell — felly ystyr «pibell» y moddau GNU yma yw'r
allbwn hwnnw'n union. Heb `--output-error`, mae allbwn safonol a
fethodd yn atal y rhediad (cyfwerth â'r offeryn GNU yn marw o
`SIGPIPE`, gyda'r rheswm wedi'i ddatgan ar y gwall safonol); gyda modd
`-nopipe` fe'i goddefir yn dawel.

Nid yw `tee -i` GNU (anwybyddu toriadau) ar gael: nid oes gan TAIRiX
osodiad signalau fesul proses i'w bennu. Daw'r switsh gyda'r gwaith
cnewyllyn hwnnw yn hytrach na chael ei dderbyn a'i anwybyddu.

## OPTIONS

- `-a, --append` — atodi at y ffeiliau a enwir; peidio â'u
  trosysgrifo.
- `-p` — goddef allbwn safonol a fethodd yn dawel; yr un fath â
  `--output-error=warn-nopipe`.
- `--output-error[=<mode>]` — sut y trinnir allbwn a fethodd. Heb
  werth, `warn-nopipe`. Y moddau (derbynnir rhagddodiad diamwys):
  `warn` — adrodd am wall ysgrifennu i unrhyw allbwn, gollwng yr
  allbwn hwnnw a pharhau; `warn-nopipe` — fel `warn`, ond gollyngir
  allbwn safonol a fethodd yn dawel ac nid yw'n effeithio ar y statws
  gadael; `exit` — adrodd am wall ysgrifennu i unrhyw allbwn a stopio;
  `exit-nopipe` — fel `exit`, ond gollyngir allbwn safonol a fethodd
  yn dawel.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.
- `--` — gorffen dosrannu opsiynau; mae pob ymresymiad diweddarach yn
  enwi ffeil, ac mae operand `-` yn enwi ffeil o'r enw `-`.

## EXAMPLES

- `ls -l | tee listing.txt` — dangos y rhestriad a chadw copi.
- `make 2>&1 | tee -a build.log` — atodi trawsgrifiad adeiladu wrth ei
  wylio.
- `cat data | tee copy1 copy2 | wc -c` — dal dau gopi a chyfrif y
  beitiau sy'n llifo ymlaen.

## EXIT STATUS

- `0` — gwasanaethwyd pob allbwn hyd ddiwedd y mewnbwn (neu
  cyflwynwyd cymorth byr y gofynnwyd amdano); nid yw methiant allbwn
  safonol a oddefir gan fodd `-nopipe` yn newid hyn.
- `1` — methodd allbwn mewn ffordd y mae'r modd a ddewiswyd yn ei
  chyfrif, neu ni ellid darllen y mewnbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cat`
- `head`
- `wc`
