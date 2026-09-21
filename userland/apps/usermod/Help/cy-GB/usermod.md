## NAME

usermod — addasu cyfrif defnyddiwr

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Mae'n newid meysydd hunaniaeth cyfrif, ei gyflwr clo, neu ei nenfwd
galluoedd. Gweithred weinyddol yw addasu cyfrif: mae'r gronfa'n gwrthod
galwr heb allu gweinyddu defnyddwyr.

Mae golygu hunaniaeth yn disodli holl set y meysydd nad ydynt yn ymwneud â
diogelwch, felly mae'r offeryn yn darllen y cofnod presennol yn gyntaf ac yn
ailanfon pob maes na enwyd gan neb heb ei newid. Gwrthodir cyfrif nad yw'r
gronfa'n ei restru cyn anfon dim.

Mae pob switsh yn weithred gronfa ei hun, a gymhwysir fesul un ac yn gyfan
neu ddim o gwbl. Mae llinell sy'n gofyn am sawl un yn anfon sawl un, mewn
trefn sefydlog — meysydd, yna galluoedd, yna clo — ac yn stopio ar y
gwrthodiad cyntaf, gan enwi'r cam a rhybuddio y gall newid cynharach fod
eisoes mewn grym.

Mae `-G` yn disodli'r set atodol gyfan yn hytrach nag ychwanegu ati: mae'r
gronfa'n cymryd setiau cyfan, a byddai ychwanegu ar sail darlleniad hen yn
waeth na disodli penodol. Cysyniad TAIRiX ei hun yw `--grants`, a
ysgrifennir ar ffurf hir yn unig; mae'r gronfa'n gwrthod unrhyw allu nad
yw'r cyfrif galw ei hun yn ei ddal.

Mae `--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn operand.

## OPTIONS

- `-c, --comment COMMENT` — sylw / enw llawn y cyfrif.
- `-d, --home HOME` — y cyfeiriadur cartref.
- `-s, --shell SHELL` — y gragen mewngofnodi.
- `-g, --gid GID` — id rhifol y prif grŵp.
- `-G, --groups LIST` — idau rhifol y grwpiau atodol, wedi'u gwahanu gan
  atalnodau, yn disodli'r set bresennol. Mae rhestr wag yn ei chlirio.
- `-L, --lock` — atal y cyfrif rhag mewngofnodi.
- `-U, --unlock` — ei ganiatáu eto.
- `--grants LIST` — enwau'r galluoedd, wedi'u gwahanu gan atalnodau, sy'n
  ffurfio'r nenfwd cyfan. Mae rhestr wag yn ei glirio.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — gosod enw llawn y cyfrif.
- `usermod -L ada` — cloi'r cyfrif.
- `usermod --grants LIST` — disodli'r nenfwd galluoedd.

## EXIT STATUS

- `0` — gwnaethpwyd pob newid a ofynnwyd amdano.
- `1` — gwrthododd neu fethodd y gronfa newid; argraffir y cam a'r rheswm ar
  y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
