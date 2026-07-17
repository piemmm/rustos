## NAME

du — amcangyfrif y gofod disg a ddefnyddir gan ffeiliau

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Yn cerdded pob operand `file` ac yn argraffu, fesul cyfeiriadur (y
dyfnaf yn gyntaf), y storfa y mae'r goeden oddi tano yn ei meddiannu,
fel `size<TAB>path`. Heb `file`, cerddir y cyfeiriadur cyfredol
(`.`). Argreffir operand `file` nad yw'n gyfeiriadur ar ei ben ei
hun.

Y mesur rhagosodedig yw storfa wirioneddol pob nod, fel y mae'r
system ffeiliau wedi'i gosod yn ei hadrodd; mae ffeiliau tenau neu
gywasgedig felly'n cyfrif yr hyn y maent yn ei feddiannu mewn
gwirionedd. Mae `--apparent-size` (neu `-b`) yn mesur yr hydoedd
ymddangosiadol mewn beitiau yn lle hynny. Argreffir meintiau mewn
blociau 1024-beit oni bai bod opsiwn uned yn dewis fel arall; mae
opsiwn uned diweddarach yn disodli un cynharach, ac mae cyfrifon
blociau'n talgrynnu i fyny (mae bloc a ddefnyddir yn rhannol yn floc
a ddefnyddir).

Adroddir llwybr na ellir ei ddarllen ar y gwall safonol ac mae'r
daith yn parhau gyda'r gweddill; nid yw cyfeiriadur annarllenadwy yn
cyfrannu dim yn hytrach na swm rhannol wedi'i ddyfalu.

Nid oes gan TAIRiX gysylltiadau caled eto, felly ni ellir cyfrif
unrhyw gofnod ddwywaith ac nid yw switshis dad-ddyblygu cysylltiadau
GNU yn bodoli; nid yw `-x` (un system ffeiliau) ar gael eto; ni
ddarllenir newidynnau amgylchedd teulu `DU_BLOCK_SIZE` — dewisir y
raddfa gan opsiynau'n unig.

## OPTIONS

- `-a, --all` — adrodd am bob ffeil hefyd, nid cyfeiriaduron yn unig.
- `-s, --summarize` — adrodd cyfanswm pob operand yn unig (yn
  gwrthdaro â `-a` a `-d`).
- `-c, --total` — atodi rhes cyfanswm cyffredinol wedi'i labelu
  `total`.
- `-d, --max-depth <n>` — adrodd am gyfeiriaduron hyd at `n` lefel
  islaw operand (`0` yn adrodd am yr operandau'n unig); nid yw'r
  cyfansymiau'n newid.
- `-S, --separate-dirs` — mae rhes cyfeiriadur yn eithrio ei
  is-gyfeiriaduron.
- `--apparent-size` — mesur hydoedd ymddangosiadol mewn beitiau, nid
  storfa a neilltuwyd.
- `-b, --bytes` — maint ymddangosiadol mewn beitiau unigol
  (`--apparent-size` gyda maint bloc o 1).
- `-k` — blociau 1024-beit (y rhagosodiad).
- `-m` — blociau 1048576-beit.
- `-h, --human-readable` — meintiau darllenadwy mewn pwerau o 1024
  (`1.0K`, `23M`).
- `--si` — meintiau darllenadwy mewn pwerau o 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — adrodd mewn blociau o `size` beit
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — gorffen pob rhes â NUL yn lle llinell newydd.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `du` — coeden y cyfeiriadur cyfredol, un rhes fesul cyfeiriadur.
- `du -sh /Users/jo` — un cyfanswm darllenadwy ar gyfer `/Users/jo`.
- `du -a docs` — pob ffeil a chyfeiriadur o dan `docs`.
- `du -d1 -c /Apps /Users` — lefel gyntaf pob storfa, yna cyfanswm
  cyffredinol.

## EXIT STATUS

- `0` — cerddwyd pob operand (neu ysgrifennwyd y cymorth byr).
- `1` — ni ellid darllen llwybr, neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
