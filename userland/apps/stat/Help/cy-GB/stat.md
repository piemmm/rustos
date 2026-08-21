## NAME

stat — adrodd statws ffeil neu system ffeiliau

## SYNOPSIS

`stat [-Lft] [-c FFORMAT | --printf=FFORMAT] [--] ffeil...`

## DESCRIPTION

Yn adrodd meysydd un statws a ddarllenwyd fesul operand, yn nhrefn y
llinell orchymyn.

**Heb `-L`, disgrifir cyswllt symbolaidd fel y mae** — dyna at beth y mae
'r offeryn hwn wrth ochr `ls`. Mae `%N` yn dangos y cyswllt a'r targed y
mae'n ei storio, mae `%F` yn dweud `symbolic link`, ac eiddo'r cyswllt ei
hun yw'r meintiau a'r stampiau amser. Mae `-L` yn datrys y cyswllt olaf
ac yn disgrifio'r hyn y mae'n ei enwi.

Mae `-f` yn newid i'r system ffeiliau sy'n dal yr operand: cyfrifon
blociau ac inodau'r gyfrol, ei maint bloc, a'r math y mae ei mowntiad yn
ei gofnodi. Mae gan y dwy ddarlleniad eirfâu meysydd **gwahanol**, felly
gwirir fformat yn erbyn yr un a ddewisir gan `-f`.

Mae `-c`/`--format` yn rendro un llinyn fformat fesul operand, ac yna
llinell newydd; mae `--printf` yn dehongli dianc ôl-slaes ac nid yw'n
ychwanegu llinell. Dyna'r unig wahaniaeth. Mae cyfarwyddeb yn derbyn
baneri a lled printf (`%-10s`, `%06i`, `%.3n`), fel y gall adroddiad
sefyll yn golofnau. `-t` yw'r ffurf gryno un llinell ar y naill
ddarlleniad neu'r llall.

Adroddir operand na ellir ei ddarllen ar y gwall safonol, disgrifir yr
operandau sy'n weddill beth bynnag, ac yna daw'r gorchymyn i ben â
statws nad yw'n sero. Mae maes na all y system hon ei gyflenwi — cipolwg
mowntio nad oes hawl iddo ei ddarllen, uid heb enw yn y cyfeiriadur
defnyddwyr — yn ymddangos fel `?` neu `UNKNOWN`, byth fel eilydd
credadwy.

Mae angen un operand o leiaf. Mae `--` yn dod â dadansoddi dewisiadau i
ben.

Mae pedwar maes yn enwi cysyniad nad yw gan TAIRiX, a **gwrthodir** hwy
wrth eu henw pan ddefnyddia fformat un ohonynt, yn lle eu llenwi â gwerth
dychmygol: `%G`, am fod yr API gwybodaeth system yn cyhoeddi cyfeiriadur
defnyddwyr a dim cymar i grwpiau, felly `%g` (y dynodydd rhifol) yw'r
maes gonest; `%t` a `%T` geirfa'r ffeil, am nad oes ffeiliau arbennig
dyfais i gael math mawr na bach; ac `%t` geirfa'r system ffeiliau, am nad
oes gan gyfrol rif hud math — mae `%T` yn enwi'r math y mae ei mowntiad
yn ei gofnodi. Digwydd y gwrthodiad wrth ddadansoddi'r fformat, cyn cyffwrdd
ag unrhyw lwybr.

Mae dau faes yn adrodd cysyniad TAIRiX yn lle un Linux. Dynodir cyfrol
gan ddynodydd 16 beit yn hytrach na rhif dyfais, felly `%d` yw'r dynodydd
hwnnw'n ddegol a `%D` yn hecsadegol; mae cymharu `%d` dwy ffeil yn dal i
ateb yn union "a ydynt ar un gyfrol?".

## OPTIONS

- `-L, --dereference` — disgrifio'r hyn y mae cyswllt symbolaidd yn ei
  enwi, yn lle'r cyswllt ei hun.
- `-f, --file-system` — disgrifio'r system ffeiliau sy'n dal pob operand
  yn lle'r operand.
- `-c, --format=FORMAT` — rendro `FFORMAT` fesul operand, ac yna llinell
  newydd.
- `--printf=FORMAT` — fel `-c`, ond gan ddehongli dianc ôl-slaes a heb
  argraffu llinell newydd olaf.
- `-t, --terse` — argraffu'r meysydd ar un llinell wedi'u gwahanu â
  bylchau.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `stat nodiadau.txt` — yr adroddiad llawn am un ffeil.
- `stat -c '%s %n' *` — maint ac enw, un llinell bob un.
- `stat -L cyswllt` — disgrifio'r hyn y mae'r cyswllt yn ei enwi.
- `stat -f .` — y gyfrol sy'n dal y cyfeiriadur gwaith.

## EXIT STATUS

- `0` — disgrifiwyd pob operand (neu ysgrifennwyd y cymorth byr).
- `1` — ni allwyd darllen un operand o leiaf, neu methodd yr allbwn.
- `2` — ni ddeallwyd y llinell orchymyn, neu enwodd ei fformat
  gyfarwyddeb na all y system hon ei gwasanaethu.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

ls, readlink, df, du
