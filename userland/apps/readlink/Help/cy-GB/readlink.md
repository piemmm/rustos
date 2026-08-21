## NAME

readlink — argraffu targed cyswllt symbolaidd

## SYNOPSIS

`readlink [-fem] [-nz] [-q | -s | -v] [--] ffeil...`

## DESCRIPTION

Yn argraffu'r targed y mae pob operand yn ei storio, un fesul operand, yn
nhrefn y llinell orchymyn.

Argreffir y targed **yn union fel y'i storiwyd**. Data yw targed cyswllt,
nid llwybr a ddatrysiwyd pan grëwyd y cyswllt: gall fod yn gymharol, gall
gynnwys `..`, ac efallai nad yw'n enwi dim o gwbl. Felly mae `readlink` yn
dangos yr ysgrifen, ac mae `ls -l` yn dangos cyswllt wrth ochr yr hyn y
mae'n ei enwi ar hyn o bryd.

Nid oes gan operand **nad** yw'n gyswllt symbolaidd unrhyw darged i'w
argraffu — gwrthodir ffeil a chyfeiriadur ill dau am yr un rheswm
«gwerth allan o'r ystod» — ac mae enw absennol yn «heb ei ddarganfod».
Y naill ffordd neu'r llall, darllenir yr operandau sy'n weddill a bydd y
gorchymyn yn gorffen â statws nad yw'n sero. Tawelwch yw'r rhagosodiad,
fel yn yr offeryn GNU: mae `-v` yn troi'r gwaith diagnosis fesul operand
ymlaen.

Mae `-n` yn gollwng yr amffinydd ar ôl y targed olaf. Gyda mwy nag un
operand fe'i hanwybyddir, ac adroddir hynny, am mai'r amffinyddion rhwng y
targedau sy'n eu gwahanu.

Mae angen un operand o leiaf. Mae `--` yn dod â dadansoddi dewisiadau i
ben.

Mae `-f`, `-e` ac `-m` yn newid i **ganoneiddio** yn lle hynny: yr unig
lwybr sy'n enwi'r hyn y mae'r operand yn datrys iddo, gyda phob cyswllt
wedi'i ddilyn a phob `..` wedi'i gymhwyso. O dan unrhyw un ohonynt nid
oes angen i'r operand fod yn gyswllt o gwbl, ac nid yw'r tri'n
gwahaniaethu ond yn faint o'r llwybr sy'n gorfod bodoli. Dewisiadau
amgen ydynt, nid addasyddion, felly y diwethaf a roddir sy'n ennill.

Y system ffeiliau sy'n berchen ar y datrysiad hwnnw — `..` corfforol,
y gyllideb naid, gwiriad hawl chwilio ar bob cyfeiriadur a groesir, a'r
rheol na all cyswllt ddatrys y tu allan i'r hyn y mae ei fowntiad yn ei
daflunio — ac mae'r offeryn hwn yn ei *alw* yn lle dilyn cysylltiadau ei
hun. Byddai ail gopi o'r algorithm a anghytunai ag un rheol yn argraffu
llwybr y mae'r system ffeiliau'n ei ddatrys yn wahanol.

## OPTIONS

- `-f, --canonicalize` — argraffu'r llwybr canonaidd; mae'n rhaid i bob
  cydran ond yr olaf fodoli.
- `-e, --canonicalize-existing` — argraffu'r llwybr canonaidd; mae'n
  rhaid i bob cydran fodoli.
- `-m, --canonicalize-missing` — argraffu'r llwybr canonaidd; nid oes
  angen i unrhyw gydran fodoli.
- `-n, --no-newline` — peidio ag argraffu'r amffinydd ar ôl y targed
  olaf (anwybyddir, gyda gair, am fwy nag un operand).
- `-z, --zero` — gorffen pob targed â NUL yn lle llinell newydd.
- `-q, -s` — peidio â gwneud diagnosis o ddarlleniad a wrthodwyd (y
  rhagosodiad; hefyd `--quiet`, `--silent`).
- `-v, --verbose` — gwneud diagnosis o ddarlleniad a wrthodwyd ar y
  gwall safonol.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — argraffu'r hyn y mae llwybr byr yn ei
  storio.
- `readlink -v alias` — ei argraffu, a dweud pam os nad yw'n gyswllt.
- `readlink -f alias` — argraffu'r hyn y mae'n datrys iddo, cysylltiadau
  a'r cwbl.
- `readlink -z a b | tr '\0' '\n'` — targedau wedi'u gwahanu â NUL i
  sgript.

## EXIT STATUS

- `0` — argraffwyd targed pob operand (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthodwyd un darlleniad o leiaf, neu methodd yr allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
