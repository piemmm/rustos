## NAME

ln — creu cysylltiadau rhwng ffeiliau

## SYNOPSIS

`ln [-srLPdFfinvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Yn creu cysylltiad symbolaidd sy'n enwi pob targed. Gydag un operand
crëir y cysylltiad yn y cyfeiriadur gwaith dan enw'r targed ei hun.
Gyda dau, mae'r ail operand yn gyfeiriadur i'w lenwi os yw'n un — neu'n
gysylltiad ag un, ac eithrio gyda `-n` — ac yn enw'r cysylltiad fel
arall. Gyda thri neu fwy, rhaid i'r olaf fod yn gyfeiriadur eisoes.

Cedwir y targed **fel y mae** ac ni chaiff ei ddatrys byth: gall fod yn
gymharol, gall gynnwys `..`, a gall enwi dim o gwbl, felly caiff
cysylltiad hongian yn ddilys. Gwirir ei ramadeg cyn ei gadw serch hynny,
felly gwrthodir targed na allai unrhyw ddatrysydd ei gerdded. Nid yw
creu cysylltiad yn rhoi unrhyw awdurdod dros yr hyn a enwir gan
hynny — awdurdodir pob defnydd diweddarach gydran wrth gydran dan eich
hunaniaeth eich hun.

Gwrthodir enw cysylltiad a gymerwyd eisoes oni bai bod `-f` neu `-i` yn
dweud ei ddisodli, ac mae'r disodli yn **tynnu** yr enw hwnnw yn gyntaf,
fel nad aiff dim drwy gysylltiad a oedd yno eisoes at yr hyn y mae'n
pwyntio ato. Ni ddisodlir cyfeiriadur byth.

Mae'r methiant cyntaf yn atal y rhediad cyn unrhyw darged diweddarach;
erys y cysylltiadau a wnaed eisoes. Mae `--` yn terfynu dadansoddi
dewisiadau: mae pob dadl ddiweddarach yn operand.

Heb `-s` mae'r cysylltiad yn un **caled**: ail gofnod cyfeiriadur ar
gyfer inode y targed ei hun. Mae'r ddau enw'n cyrraedd un ffeil, mae
ysgrifen trwy'r naill i'w gweld trwy'r llall, ac erys storfa'r ffeil
nes tynnu'r enw olaf. Rhaid i'r ddau enw fod ar un gyfrol, ac ni roddir
ail enw i gyfeiriadur byth — am fod y goeden ffeiliau'n aros yn goeden
y mae `..` yn golygu'r cyfeiriadur y daethpwyd trwyddo mewn gwirionedd.

Mae `-r` yn storio targed cyswllt symbolaidd yn gymharol i gyfeiriadur y
cyswllt ei hun. Mae'r system ffeiliau'n canoneiddio'r ddwy hanner yn
gyntaf, felly mae'r gwahaniaeth rhyngddynt yn union: nid yw dau lwybr
canonaidd yn cynnwys `..` na chyswllt. Byddai'r un rhifyddeg ar yr
operandau fel y'u hysgrifennwyd yn enwi gwrthrych gwahanol cyn gynted ag
y byddai cysylltiad yn y cwestiwn. Mae angen `-s` ar `-r`, am nad yw
cyswllt caled yn storio targed i'w wneud yn gymharol.

Gwrthodir `-b`/`-S` am nad oes peiriannwaith wrth gefn i'w alw.

## OPTIONS

- `-s, --symbolic` — creu cysylltiadau symbolaidd yn lle rhai caled.
- `-r, --relative` — storio targed pob cyswllt symbolaidd yn gymharol i
  gyfeiriadur y cyswllt ei hun. Mae angen `-s`.
- `-L, --logical` — cysylltu'n galed yr hyn y mae targed symbolaidd
  yn ei enwi, yn hytrach na'r cysylltiad ei hun.
- `-P, --physical` — cysylltu'n galed y targed fel y'i sillafwyd, heb
  ddilyn cysylltiad symbolaidd terfynol. Rhagosodiad.
- `-d, -F, --directory` — derbyn operand cyfeiriadur. Gwrthodir y
  cysylltiad serch hynny: ni chaiff unrhyw ddefnyddiwr roi ail enw i
  gyfeiriadur.
- `-f, --force` — tynnu enw cysylltiad presennol, ac wedyn creu'r
  cysylltiad.
- `-i, --interactive` — gofyn cyn tynnu enw cysylltiad presennol; dim
  ond ateb sy'n dechrau â `y`/`Y` sy'n cydsynio. Yr olaf o `-f` a `-i`
  sy'n ennill.
- `-n, --no-dereference` — trin cyrchfan sy'n gysylltiad symbolaidd at
  gyfeiriadur fel yr enw syml y mae hefyd, yn hytrach na chyfeiriadur i
  greu'r cysylltiadau ynddo.
- `-v, --verbose` — adrodd pob cysylltiad a wnaed fel
  `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — creu pob cysylltiad yn `dir`, sy'n
  rhaid iddo fod yn gyfeiriadur eisoes. Daw'r gwerth ynghlwm (`-tdir`,
  `--target-directory=dir`) neu fel y ddadl nesaf.
- `-T, --no-target-directory` — trin y cyrchfan fel enw cysylltiad, byth
  fel cyfeiriadur i'w lenwi; dau operand yn union. Ni cheir ei gyfuno â
  `-t`.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — cysylltu enw â bwndel.
- `ln -s ../shared/notes.txt` — cysylltu `notes.txt` yma â tharged
  cymharol.
- `ln -sv -t Links a.txt b.txt` — cysylltu'r ddau ffeil i `Links`, gan
  adrodd pob cysylltiad.
- `ln -sfn /Storage/media Music` — ailgyfeirio cysylltiad `Music`
  presennol at gyfeiriadur newydd, gan ddisodli'r cysylltiad yn lle
  cysylltu y tu mewn iddo.

## EXIT STATUS

- `0` — crëwyd pob cysylltiad (neu ysgrifennwyd y cymorth byr); nid yw
  cwestiwn `-i` a wrthodwyd yn fethiant.
- `1` — unrhyw beth arall, gyda'r rheswm ar y llif gwallau. Mae llinell
  orchymyn na ddeallwyd hefyd yn gorffen â `1`.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
