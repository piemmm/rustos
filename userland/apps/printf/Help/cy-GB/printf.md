## NAME

printf — fformatio ac argraffu data

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Yn argraffu'r `argument`(au) dan reolaeth `format`, fel y ffwythiant C
`printf`. Mae'r fformat yn cynnwys tri math o elfen: nodau cyffredin, a
gopïir i'r allbwn safonol; dilyniannau dianc â slaes wrthol; a
chyfarwyddebau trosi `%`, pob un yn trosi'r ddadl nesaf.

Y dilyniannau dianc yw `\a` (rhybudd), `\b` (ôl-nod), `\c` (gorffen yr
holl allbwn ar unwaith), `\e` (dianc), `\f` (porthiant tudalen), `\n`
(llinell newydd), `\r` (dychweliad cludwr), `\t` (tab), `\v` (tab
fertigol), `\\`, `\"`, `\NNN` (un i dri digid wythol), `\xHH` (un neu
ddau ddigid hecsadegol) a `\uHHHH` / `\UHHHHHHHH` (pwyntiau côd Unicode,
pedwar neu wyth digid hecsadegol).

Y trosiadau yw `%d`/`%i` (degol ag arwydd), `%u` (degol heb arwydd),
`%o`/`%x`/`%X` (wythol a hecsadegol), `%e`/`%E`/`%f`/`%F`/`%g`/`%G`/
`%a`/`%A` (pwynt arnawf), `%c` (nod cyntaf y ddadl), `%s` (llinyn),
`%b` (llinyn y dehonglir ei ddilyniannau dianc ei hun, ysgrifennir yr
wythol `\0NNN`), `%q` (llinyn wedi'i ddyfynnu i'w ailddefnyddio fel
mewnbwn cragen) a `%%` (`%` llythrennol). Mae cyfarwyddeb yn derbyn
baneri C `-`, `+`, bwlch, `#`, `0` a `'`, lled maes a manwl gywirdeb;
gall y lled a'r manwl gywirdeb fod yn `*`, gan ddarllen eu gwerth o'r
ddadl nesaf. Nid yw `%b` na `%q` yn derbyn baneri, lled na manwl
gywirdeb.

Ailddefnyddir y fformat hyd nes y treulir pob dadl; mae trosiad heb
ddadl ar ôl yn argraffu sero neu'r llinyn gwag. Darllenir dadl rifol
fel rhif C (hecsadegol `0x`, wythol â `0` blaen, pwynt arnawf, `inf`,
`nan`); mae `'` neu `"` blaen yn trosi pwynt côd y nod canlynol. Caiff
dadl nad yw'n rhif, sydd ond yn rhannol yn rhif, neu sydd y tu hwnt i'r
ystod ei diagnosio ar yr allbwn gwall a'i throsi cyn belled ag y bo modd
— mae'r rhediad yn parhau ac yn gorffen â statws `1`. Mae trosiad
anhysbys, baner ar drosiad nad yw'n ei derbyn, neu ddilyniant dianc
gwallus yn gorffen y rhediad â diagnosis.

Dau wyriad bwriadol oddi wrth `printf` GNU: cyfrifir pwynt arnawf mewn
manwl gywirdeb dwbl IEEE 754 (mae GNU yn defnyddio `long double`), felly
mae gwerth y tu hwnt i ystod double yn argraffu `inf`; ac mae dadl
*gyntaf* o `-h` neu `-?` yn dangos y cymorth byr hwn — ysgrifennir
fformat o'r fath `printf -- -h...`.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn (fel dadl gyntaf yn
  unig).
- `--` — gorffen dosrannu opsiynau; y ddadl nesaf yw'r fformat.

## EXAMPLES

- `printf '%s\n' hello` — argraffu `hello` a llinell newydd.
- `printf '%d\n' 0x10` — argraffu `16`.
- `printf '%5.2f|\n' 3.14159` — argraffu ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — argraffu
  `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — argraffu dwy linell o un ddadl.
- `printf '%s-' a b c` — ailddefnyddio'r fformat: `a-b-c-`.

## EXIT STATUS

- `0` — ysgrifennwyd popeth (neu'r cymorth byr y gofynnwyd amdano).
- `1` — diagnosiwyd problem drosi, roedd y fformat ar goll neu'n
  annilys, roedd dilyniant dianc yn wallus, neu peidiodd yr allbwn â
  derbyn beitiau.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 megis
  `cy-GB`).

## SEE ALSO

- `seq`
- `man`
