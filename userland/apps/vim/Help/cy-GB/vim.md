## NAME

vim — y golygydd testun moddol

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Mae'n golygu ffeiliau testun gyda set orchmynion foddol y golygydd
vim adnabyddus. Mae'r sesiwn yn dechrau yn y modd normal: gorchmynion
yw'r bysellau, ac mae `i` (neu `a`, `o` a'u hamrywiadau) yn mynd i'r
modd mewnosod lle daw teipio'n destun. Mae `Esc` yn dychwelyd i'r
modd normal. Mae `:q` yn gadael; mae `:wq` (neu `ZZ`) yn ysgrifennu
ac yn gadael.

Gellir enwi sawl ffeil; mae'r sesiwn yn agor y gyntaf ac mae `:n` /
`:prev` yn camu trwy restr yr ymresymiadau. Ffeil nad yw'n bodoli eto
yw `[New File]`, a grëir ar yr ysgrifennu cyntaf.

Gorchmynion y modd normal (y craidd vim a weithredwyd):

- Symudiadau: `h j k l`, y saethau, `w W b B e E`, `0 ^ $`,
  `f F t T` gydag ailadroddion `;`/`,`, `gg G`, `{ }`, `%`, `H M L`,
  ac `Enter`. Mae rhagddodiad cyfrif yn ailadrodd symudiad: `3w`.
- Gweithredyddion: `d` (dileu), `c` (newid), `y` (tynnu copi), wedi'u
  cymhwyso dros unrhyw symudiad neu wrthrych testun
  (`iw aw i( a( i[ i{ i" i' i<` a'u parau); wedi'u dyblu
  (`dd cc yy`) maent yn gweithredu ar linellau cyfan. Byrfoddau:
  `x X s S D C Y r ~ J`.
- Cofrestrau: mae `"a`–`"z` cyn gweithredydd neu put yn dewis
  cofrestr enwedig; mae priflythrennau'n atodi. Mae `p`/`P` yn gosod
  ar ôl/cyn y cyrchwr.
- Hanes dadwneud: mae `u` yn dadwneud newidiadau cyfan, `Ctrl-R` yn
  ail-wneud, a `.` yn ailadrodd y newid diwethaf (gan gynnwys ei
  destun a fewnosodwyd).
- Chwilio: `/pattern` ymlaen, `?pattern` yn ôl, `n`/`N` yn ailadrodd,
  `*` yn dod o hyd i'r gair o dan y cyrchwr. Mae patrymau'n cynnal
  llythrenolion, `.`, `*`, `^`, `$`, dosbarthau `[...]`, a therfynau
  gair `\<` `\>`. Erys cydweddiadau wedi'u hamlygu tan `:noh`.
- Dewisiad gweledol: `v` (nodau) a `V` (llinellau), wedi'u hestyn gan
  unrhyw symudiad neu wrthrych testun, yna gweithredir arnynt gyda
  `d x c s y J`.
- Sgrolio: `Ctrl-D Ctrl-U` (hanner ffenestr), `Ctrl-F Ctrl-B` a
  PageUp/PageDown (ffenestr lawn); mae `Ctrl-G` yn dangos crynodeb y
  ffeil.

Craidd gorchmynion ex (`:`): `:w [file]`, `:q`, `:wq`, `:x`,
`:e file`, `:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, cyfeiriadau llinell (`:12`, `:$`, `:.+2`),
`:[range]d`, a `:[range]s/pattern/replacement/[g]` (gyda `&` am y
cydweddiad cyfan yn yr amnewidiad, `%` am bob llinell yn yr ystod).
Mae `!` ar ôl `w`, `q` neu `e` yn gorfodi heibio'r osgo darllen-yn-
unig neu newidiadau heb eu hysgrifennu.

Mae popeth y mae vim yn ei gludo y tu hwnt i'r craidd hwn wedi'i
raddoli ar gyfer camau diweddarach; mae'r rhestr raddoli'n byw yn
`plans/VIM.md` yng nghoeden y ffynhonnell.

## OPTIONS

- `-R` — darllen-yn-unig: mae'r byffer yn golygu yn y cof ond
  gwrthodir `:w` oni orfodir gyda `:w!`.
- `+num` — dechrau ar linell `num` y ffeil gyntaf.
- `+` — dechrau ar linell olaf y ffeil gyntaf.
- `+/pattern` — dechrau ar gydweddiad cyntaf `pattern` yn y ffeil
  gyntaf.
- `--` — diwedd yr opsiynau; enw ffeil yw pob ymresymiad dilynol.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

- `0` — daeth y sesiwn i ben gyda gorchymyn gadael, neu dangoswyd y
  cymorth byr.
- `1` — methodd y derfynell; argraffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).
- `TERM` — proffil y derfynell y mae'r sesiwn yn ei yrru; mae
  gwerthoedd anhysbys yn dirywio i'r sylfaen «dumb».

## SEE ALSO

- `man`
- `cat`
