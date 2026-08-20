## NAME

ln — creu cysylltiadau symbolaidd

## SYNOPSIS

`ln -s [-finvT] [-t dir] [--] target... [link_name]`

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

Mae `-s` yn ofynnol ar y system hon, sydd heb gysylltiadau caled: heb
hynny nid oes dim i `ln` ei greu, ac fe ddywed hynny yn lle creu
cysylltiad symbolaidd, sy'n wrthrych gwahanol. Gwrthodir y dewisiadau
cysylltiad-caled yn unig `-L`, `-P`, `-d` ac `-F` am yr un rheswm.
Gwrthodir `-b`/`-S` am nad oes peiriannwaith wrth gefn i'w alw, a `-r`
am fod cyfrifo targed cymharol i gyfeiriadur y cysylltiad yn galw am
ddatrysiad canoneiddio nad yw'r system hon yn ei gynnig — byddai un
geiriadurol yn enwi gwrthrych gwahanol cyn gynted ag y byddai
cysylltiad yn y cwestiwn.

## OPTIONS

- `-s, --symbolic` — creu cysylltiadau symbolaidd. Gofynnol: gweler
  uchod.
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
