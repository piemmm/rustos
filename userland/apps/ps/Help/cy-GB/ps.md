## NAME

ps — rhestru prosesau

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Mae'n rhestru prosesau trwy API Gwybodaeth y System. Yn ragosodedig
dim ond prosesau'r galwr ei hun a restrir; mae'r gwasanaeth yn
cymhwyso pob cwmpas ymholiad yn erbyn hunaniaeth y galwr a ardystiwyd
gan y cnewyllyn, ac nid oes llwybr sy'n osgoi'r gwiriad hwnnw.

Argreffir pob proses fel un rhes o dan bennawd colofnau: id y broses
(`PID`), id y broses riant (`PPID`), idau'r defnyddiwr a'r grŵp sy'n
berchen (`UID`, `GID`), y cyflwr amserlennu (`S`), y CPU y rhedodd y
broses arno ddiwethaf (`CPU`), ac enw'r gorchymyn (`NAME`).

Nid yw `ps` yn cymryd operandau.

## OPTIONS

- `-e, -A, --all` — rhestru pob proses ar y system yn hytrach na rhai'r
  galwr yn unig; dim ond i alwr sy'n dal `CAP_SYSINFO_GLOBAL` y mae'r
  gwasanaeth yn caniatáu'r olwg hon.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `ps` — rhestru eich prosesau eich hun.
- `ps -e` — rhestru pob proses ar y system.

## EXIT STATUS

- `0` — ysgrifennwyd y rhestriad.
- `1` — gwrthododd neu fethodd y gwasanaeth, neu ni ellid cyflwyno'r
  rhestriad.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
