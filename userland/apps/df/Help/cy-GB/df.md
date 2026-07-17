## NAME

df — adrodd ar ddefnydd gofod y systemau ffeiliau

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Yn adrodd, un rhes fesul system ffeiliau wedi'i gosod, maint y
gyfrol, y gofod a ddefnyddiwyd, y gofod sydd ar gael, y ganran a
ddefnyddiwyd a'r pwynt gosod. Gydag operandau `file`, adroddir yn lle
hynny ar y system ffeiliau sy'n cynnwys pob operand (un rhes fesul
system ffeiliau, faint bynnag o operandau y mae'n eu cwmpasu).

Daw'r ffigurau o restr gosodiadau'r API gwybodaeth system, fel y mae
pob gyrrwr system ffeiliau wedi'i osod yn adrodd ei gyfrifon ei hun.
Yn ragosodedig mae'r adroddiad yn cuddio gosodiadau heb gapasiti eu
hunain (rhwymiadau golwg synthetig y system) a gosodiadau pellach o
gyfrol a restrwyd eisoes; mae `-a` yn dangos popeth, a nodir nifer y
cofnodion cudd ar y ffrwd wybodaeth safonol (fd 3), byth yn y tabl.

Argreffir meintiau mewn blociau 1024-beit oni bai bod opsiwn uned yn
dewis fel arall; mae opsiwn uned diweddarach yn disodli un cynharach,
ac mae cyfrifon blociau'n talgrynnu i fyny. Mae system ffeiliau y
mae ei fformat yn neilltuo inodau yn ôl y galw yn adrodd ffigurau
inodau sero o dan `-i` — yr ateb gonest «heb ei olrhain».

Adroddir operand `file` nad yw'n bodoli, neu sy'n llwybr cymharol
(mae pwyntiau gosod yn absoliwt; nid yw `df` byth yn dyfalu
datrysiad), ar y gwall safonol ac mae'r adroddiad yn parhau gyda'r
gweddill. Nid yw opsiynau GNU `--output`, `--sync` a `--no-sync` ar
gael eto.

## OPTIONS

- `-a, --all` — cynnwys y gosodiadau di-gapasiti a dyblyg y mae'r
  rhagosodiad yn eu cuddio.
- `-T, --print-type` — ychwanegu colofn math y system ffeiliau.
- `-t, --type <type>` — adrodd am systemau ffeiliau o'r math `type`
  yn unig (ailadroddadwy).
- `-x, --exclude-type <type>` — hepgor systemau ffeiliau o'r math
  `type` (ailadroddadwy).
- `-i, --inodes` — adrodd ar gyfrifon inodau yn lle defnydd blociau.
- `-P, --portability` — fformat cludadwy POSIX (penawdau
  `1024-blocks` a `Capacity`).
- `-l, --local` — cyfyngu'r adroddiad i systemau ffeiliau lleol (pob
  gosodiad TAIRiX heddiw: ni hidlir dim).
- `--total` — atodi rhes wedi'i labelu `total` sy'n adio'r ffigurau a
  ddangosir.
- `-k` — blociau 1024-beit (y rhagosodiad).
- `-h, --human-readable` — meintiau darllenadwy mewn pwerau o 1024
  (`1.0K`, `23M`).
- `-H, --si` — meintiau darllenadwy mewn pwerau o 1000 (`1.0k`,
  `23M`).
- `-B, --block-size <size>` — adrodd mewn blociau o `size` beit
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `df` — defnydd pob cyfrol go iawn mewn blociau 1024-beit.
- `df -h` — yr un peth, mewn meintiau darllenadwy.
- `df /Users/jo` — y system ffeiliau sy'n cynnwys `/Users/jo`.
- `df -aT` — pob gosodiad, gyda'i fath system ffeiliau.
- `df --total -k` — y cyfrolau ynghyd â rhes `total` wedi'i hadio.

## EXIT STATUS

- `0` — cwmpasodd yr adroddiad bopeth a ofynnwyd (neu ysgrifennwyd y
  cymorth byr).
- `1` — ni ellid adrodd am operand, ni adawodd yr hidlwyr ddim, neu
  methodd yr ymholiad neu'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
