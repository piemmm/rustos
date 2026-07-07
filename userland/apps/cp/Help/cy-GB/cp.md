## NAME

cp — copïo ffeiliau a chyfeiriaduron

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Mae'n copïo pob operand ffynhonnell i gyrchfan. Gydag un ffynhonnell a
chyrchfan nad yw'n enwi cyfeiriadur, copïir y ffynhonnell i'r union
lwybr hwnnw. Pan fo'r gyrchfan yn enwi cyfeiriadur sy'n bodoli — a bob
amser pan fo mwy nag un ffynhonnell — copïir pob ffynhonnell *i mewn*
i'r cyfeiriadur hwnnw o dan ei henw sail ei hun.

Dim ond gydag `-r` y copïir ffynhonnell sy'n gyfeiriadur, sy'n
atgynhyrchu'r is-goeden gyfan; heb `-r` gwrthodir operand cyfeiriadur.
Trosysgrifir ffeil gyrchfan sy'n bodoli yn ragosodedig, fe'i hepgorir
o dan `-n`, a gofynnir amdani ar ffrwd y gwall safonol o dan `-i`
(mae cwestiwn a wrthodwyd yn hepgor y copi hwnnw heb wall; ni thrinnir
ateb annarllenadwy byth fel cydsyniad).

Mae'r methiant cyntaf yn atal y rhediad cyn unrhyw operand
diweddarach. Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad
diweddarach yn llwybr.

## OPTIONS

- `-r, -R, --recursive` — copïo cyfeiriaduron a'u cynnwys.
- `-f, --force` — pan na ellir creu ffeil gyrchfan, ei dileu a
  cheisio'r copi unwaith eto.
- `-i, --interactive` — gofyn cyn trosysgrifo ffeil sy'n bodoli; dim
  ond ateb yn dechrau ag `y`/`Y` sy'n cydsynio.
- `-n, --no-clobber` — peidio byth â throsysgrifo ffeil sy'n bodoli.
  Y diweddaraf o `-i` ac `-n` sy'n ennill.
- `-v, --verbose` — adrodd am bob copi fel `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copïo pob ffynhonnell i `dir`,
  y mae'n rhaid iddo fod yn gyfeiriadur sy'n bodoli. Daw'r gwerth
  ynghlwm (`-tdir`, `--target-directory=dir`) neu fel yr ymresymiad
  nesaf.
- `-T, --no-target-directory` — trin y gyrchfan fel ffeil gyffredin;
  caniateir un ffynhonnell yn union. Ni ellir ei gyfuno ag `-t`.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `cp notes.txt backup.txt` — copïo un ffeil i enw newydd.
- `cp -r Projects Archive` — atgynhyrchu coeden `Projects` y tu mewn i
  `Archive` (neu fel `Archive` os nad yw'n bodoli).
- `cp -v -t Backup a.txt b.txt` — copïo'r ddwy ffeil i `Backup`, gan
  adrodd am bob copi.

## EXIT STATUS

- `0` — llwyddodd pob copi (nid yw hepgoriad `-n` na chwestiwn `-i` a
  wrthodwyd yn fethiannau).
- `1` — methiant system ffeiliau, anogwr neu allbwn; argraffir y
  rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
