## NAME

mv — symud (ailenwi) ffeiliau a chyfeiriaduron

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Mae'n symud pob operand ffynhonnell i gyrchfan. Gydag un ffynhonnell a
chyrchfan nad yw'n enwi cyfeiriadur, ailenwir y ffynhonnell i'r union
lwybr hwnnw. Pan fo'r gyrchfan yn enwi cyfeiriadur sy'n bodoli — a bob
amser pan fo mwy nag un ffynhonnell — symudir pob ffynhonnell *i mewn*
i'r cyfeiriadur hwnnw o dan ei henw sail ei hun.

Ailenwad atomig sy'n cadw hunaniaeth y nod yw symudiad o fewn un
gyfrol. Ni all symudiad y mae ei ffynhonnell a'i gyrchfan ar gyfrolau
gwahanol fod yn atomig: mae'n syrthio'n ôl ar gopïo'r ffynhonnell i'r
gyrchfan ac yna dileu'r ffynhonnell (atgynhyrchir cyfeiriaduron yn
ailadroddus).

Trosysgrifir cyrchfan sy'n bodoli yn ragosodedig, fe'i hepgorir o dan
`-n`, a gofynnir amdani ar ffrwd y gwall safonol o dan `-i` (mae
cwestiwn a wrthodwyd yn hepgor y symudiad hwnnw heb wall; ni thrinnir
ateb annarllenadwy byth fel cydsyniad). Mae'r methiant cyntaf yn atal
y rhediad cyn unrhyw operand diweddarach. Mae `--` yn gorffen dosrannu
opsiynau: mae pob ymresymiad diweddarach yn llwybr.

## OPTIONS

- `-f, --force` — dileu cyrchfan sy'n rhwystro a cheisio'r ailenwad
  eto; peidio byth â holi. Y diweddaraf o `-f`, `-i` ac `-n` sy'n
  ennill.
- `-i, --interactive` — gofyn cyn trosysgrifo cyrchfan sy'n bodoli;
  dim ond ateb yn dechrau ag `y`/`Y` sy'n cydsynio.
- `-n, --no-clobber` — peidio byth â throsysgrifo cyrchfan sy'n
  bodoli.
- `-v, --verbose` — adrodd am bob symudiad fel
  `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — symud pob ffynhonnell i `dir`, y
  mae'n rhaid iddo fod yn gyfeiriadur sy'n bodoli. Daw'r gwerth
  ynghlwm (`-tdir`, `--target-directory=dir`) neu fel yr ymresymiad
  nesaf.
- `-T, --no-target-directory` — trin y gyrchfan fel ffeil gyffredin;
  caniateir un ffynhonnell yn union. Ni ellir ei gyfuno ag `-t`.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `mv draft.txt final.txt` — ailenwi un ffeil.
- `mv -v a.txt b.txt Archive` — symud y ddwy ffeil i `Archive`, gan
  adrodd am bob symudiad.
- `mv -n new.cfg current.cfg` — gosod ffeil dim ond os nad yw'r
  gyrchfan yn bodoli eisoes.

## EXIT STATUS

- `0` — llwyddodd pob symudiad (nid yw hepgoriad `-n` na chwestiwn
  `-i` a wrthodwyd yn fethiannau).
- `1` — methiant system ffeiliau, anogwr neu allbwn; argraffir y
  rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
