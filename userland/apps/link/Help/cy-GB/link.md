## NAME

link — rhoi ail enw i ffeil

## SYNOPSIS

`link [--] presennol newydd`

## DESCRIPTION

Yn creu cyswllt caled: daw `newydd` yn ail enw i'r nod y mae `presennol`
yn ei enwi eisoes. Mae'r ddau enw wedyn yn cyrraedd yr un ffeil — mae
ysgrifennu trwy'r un yn weladwy trwy'r llall, am fod un ffeil ac nid
copi — ac mae storfa'r ffeil yn goroesi hyd nes tynnu'r olaf o'i henwau.

Yn fwriadol nid oes dewisiadau. `ln` yw'r offeryn sydd â `-f`, `-i`,
`-v`, `-s`, `-L`/`-P` a ffurfiau cyrchfan `-t`/`-T`; mae eu cadw ar
wahân yn golygu bod sgript sy'n gorfod creu un cyswllt caled a dim arall
yn cael offeryn na all ddisodli enw, dilyn cyswllt, na chreu un
symbolaidd yn ei le.

Ni ddilynir y naill enw na'r llall. `presennol` yw'r nod **fel y'i
teipiwyd**, felly ni all cyswllt symbolaidd a blannwyd yno ailgyfeirio'r
enw newydd at ei darged (`ln -L` yw'r offeryn ar gyfer y safiad dilyn).
Enw sy'n cael ei greu yw `newydd`: gwrthodir un llawn, ni ddisodlir ef
byth.

Mae pob gwrthodiad yn dweud rhywbeth gwahanol:

- mae'r enw newydd yn bod eisoes — nid yw creu erioed yn disodli enw;
- mae `presennol` yn **gyfeiriadur** — mae gan gyfeiriadur un enw yn
  union bob amser, felly ni all unrhyw brifsawdd roi ail iddo;
- mae'r ddau enw ar **gyfrolau gwahanol** — rhaid i ail enw nod fod ar y
  gyfrol sy'n ei storio;
- byddai cyfrif enwau'r fformat fesul nod yn gorlifo;
- mae'r system ffeiliau'n storio **un enw fesul nod** — nodwedd barhaol
  i'r fformat hwnnw, nid methiant dros dro. Defnyddiwch `ln -s` am
  gyswllt symbolaidd yno.

Mae angen dau operand yn union; mae unrhyw beth arall yn wall defnydd ac
ni chreir cyswllt. Mae `--` yn dod â dadansoddi dewisiadau i ben.

## OPTIONS

- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `link adroddiad.txt adroddiad-copi.txt` — ail enw i un ffeil.
- `link -- -enw-rhyfedd ail` — cysylltu enw sy'n dechrau â chysylltnod.

## EXIT STATUS

- `0` — crëwyd y cyswllt (neu ysgrifennwyd y cymorth byr).
- `1` — gwrthododd y system ffeiliau'r cyswllt, neu methodd yr allbwn;
  argreffir y rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
