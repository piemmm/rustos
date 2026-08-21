## NAME

readlink — argraffu targed cyswllt symbolaidd

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] ffeil...`

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

Gwrthodir dewisiadau canoneiddio GNU `-f`, `-e` ac `-m`, heb eu
brasamcanu. Datrys pob cydran llwybr — dilyn pob cyswllt, trin `..` yn
gorfforol, gorfodi'r gyllideb naid a'r rheol na all cyswllt ddianc o'r
gyfrol sy'n ei storio — yw unig weithrediad y system ffeiliau. Gallai ail
gopi yma argraffu llwybr y mae'r system ffeiliau'n ei ddatrys yn wahanol,
felly mae'r dewisiad yn methu hyd nes bod y system ffeiliau'n cynnig y
datrysiad ei hun.

## OPTIONS

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
- `readlink -z a b | tr '\0' '\n'` — targedau wedi'u gwahanu â NUL i
  sgript.

## EXIT STATUS

- `0` — argraffwyd targed pob operand (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthodwyd un darlleniad o leiaf, neu methodd yr allbwn.
- `2` — ni ddeallwyd y llinell orchymyn, neu enwodd ddewisiad
  canoneiddio.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
