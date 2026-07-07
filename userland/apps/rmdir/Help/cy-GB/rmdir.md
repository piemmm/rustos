## NAME

rmdir — dileu cyfeiriaduron gwag

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

Mae'n dileu pob operand cyfeiriadur, yn eu trefn. Dim ond
**cyfeiriadur gwag** a ddilëir: mae'r system ffeiliau ei hun yn
gwrthod ffeil (neu unrhyw beth nad yw'n gyfeiriadur) a chyfeiriadur â
chynnwys, yn atomig, felly ni ellir byth ddatgysylltu dim arall yn ei
le. Defnyddiwch `rm` ar gyfer ffeiliau ac `rm -r` ar gyfer coed â
chynnwys.

Gydag `-p` dilëir hynafiaid pob operand hefyd, y mwyaf mewnol yn
gyntaf: mae `rmdir -p a/b/c` yn dileu `a/b/c`, yna `a/b`, yna `a`. Ni
ofynnir byth am ddileu gwreiddyn noeth llwybr (`/` neu wreiddyn alias
fel `Home:/`).

Gyda `--ignore-fail-on-non-empty` nid yw gwrthodiad «cyfeiriadur heb
fod yn wag» yn wall — mae'r operand (neu gerddediad `-p`) yn stopio
yno. Ni oddefir unrhyw wrthodiad arall. Mae'r methiant gwirioneddol
cyntaf yn atal y rhediad cyn unrhyw operand diweddarach. Mae `--` yn
gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn llwybr.

## OPTIONS

- `-p, --parents` — dileu hynafiaid pob operand hefyd, y mwyaf mewnol
  yn gyntaf.
- `-v, --verbose` — adrodd am bob ymgais dileu fel
  `rmdir: removing directory, 'dir'`.
- `--ignore-fail-on-non-empty` — nid yw cyfeiriadur nad yw'n wag yn
  wall; gydag `-p` mae'r cerddediad i fyny'n stopio yno.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun (hefyd
  `--help`).

## EXAMPLES

- `rmdir Scratch` — dileu un cyfeiriadur gwag.
- `rmdir -p Projects/os/build` — dileu'r gadwyn, y mwyaf mewnol yn
  gyntaf.
- `rmdir -p --ignore-fail-on-non-empty a/b` — dileu `a/b`, ac `a`
  hefyd os yw hynny'n ei adael yn wag.

## EXIT STATUS

- `0` — llwyddodd pob dileu (nid yw gwrthodiad a oddefir gan
  `--ignore-fail-on-non-empty` yn fethiant).
- `1` — methiant system ffeiliau neu allbwn; argraffir y rheswm ar y
  gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

mkdir, rm, ls
