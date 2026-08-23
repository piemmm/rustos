## NAME

telnet — cleient y Derfynell Rithwir Rwydwaith (RFC 854)

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Yn agor cysylltiad TCP at westeiwr ac yn trosglwyddo'r derfynell iddo: mae
allbwn y gwesteiwr yn ymddangos ar yr allbwn safonol, mae'r bysellau'n mynd
at y gwesteiwr, ac mae'r nod dianc (`^]` yn ddiofyn) yn agor y dehonglydd
gorchmynion `telnet>`. Heb westeiwr, mae `telnet` yn cychwyn wrth yr anogwr
hwnnw ac mae `open` yn cysylltu.

Dyma'r ffordd i gyrraedd gwasanaeth llinell-wrth-linell ar beiriant arall, a
hefyd y ffordd i holi unrhyw wasanaeth TCP â llaw: mae `telnet host 80` yn
agor cysylltiad y gallwch deipio cais iddo.

Gall y gwesteiwr fod yn enw neu'n gyfeiriad IPv4/IPv6 llythrennol. Caiff enw
ei ddatrys gan ddatryswr sylfaenol y system, sy'n darllen y gweinyddion DNS
ymgylchol wedi'u ffurfweddu drwy'r API gwybodaeth system. Rhif yw'r porth:
nid oes cronfa ddata gwasanaethau, felly mae *enw* gwasanaeth yn wall
defnydd yn hytrach na chwympo'n ddistaw yn ôl at borth 23.

Mae negodi dewisiadau'n dilyn RFC 855 gyda disgyblaeth ddi-ddolen RFC 1143,
felly nid yw cymar sy'n ailadrodd byth yn gwneud i'r cleient ailadrodd. Y
dewisiadau a weithredir yw BINARY, ECHO, SUPPRESS GO AHEAD, STATUS, TIMING
MARK, TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL, LINEMODE a
NEW-ENVIRON; caiff unrhyw un arall ei wrthod, sef ystyr dewisiad heb ei
weithredu. Gweithredir LINEMODE (RFC 1184) yn llawn — y mwgwd `MODE`, y tabl
nodau lleol (SLC) a `FORWARDMASK` — felly mae'r cleient yn golygu'r llinell
fel y mae'r gweinydd yn gofyn, gyda'r nodau y mae'r gweinydd yn eu negodi.

Adroddir maint y ffenestr drwy NAWS wrth gysylltu ac eto pan fydd yn newid.
Nid oes signal newid maint yn TAIRiX, felly darllenir y maint eto bob tro y
byddwch yn teipio; mae newid maint yn cyrraedd y gwesteiwr wrth eich bysell
nesaf.

Nid yw `NEW-ENVIRON` yn datgelu **ond** y newidynnau a ddiffiniwch ac a
allforiwch â'r gorchymyn `environ`; nid yw'r cleient byth yn anfon ei
amgylchedd ei hun. Mae `-a` a `-l` yn allforio enw mewngofnodi, a dyna'r un
peth y mae galwad yn ei ddatgelu ohono'i hun.

Mae dau orchymyn o'r offeryn hanesyddol yn absennol yn fwriadol. Nid oes
dianc i'r gragen `!`: ni roddir i raglen sy'n dosrannu data rhwydwaith
gelyniaethus yr awdurdod i gychwyn cragen. Nid oes `slc check`, oherwydd nid
yw RFC 1184 yn rhoi iddo ffurf ar y wifren sy'n wahanol i `slc export`. Nid
yw'r rhyngwyneb soced yn datgelu data brys TCP, felly mae Synch yn teithio
fel y Data Mark yn unig. Pan fydd y mewnbwn safonol yn cyrraedd diwedd y
ffeil — galwad wedi'i hailgyfeirio fel `telnet host 80 < cais` — dim ond ochr
anfon a gaiff ei chau, ac mae'r sesiwn yn parhau i ddarllen hyd nes y bydd y
gwesteiwr pell yn cau hefyd, felly ni chaiff yr ymateb ei daflu fel y mae'r
offeryn hanesyddol yn ei wneud.

## OPTIONS

- `-4, --ipv4` — cysylltu dros IPv4 yn unig.
- `-6, --ipv6` — cysylltu dros IPv6 yn unig.
- `-8, --binary` — gofyn am lwybr data 8-did i'r ddau gyfeiriad.
- `-L, --eight-bit-output` — gofyn am lwybr 8-did ar yr allbwn yn unig.
- `-E, --no-escape` — dim nod dianc; mae popeth yn mynd at y gwesteiwr.
- `-e, --escape <char>` — gosod y nod dianc (`^]`, `^A`, un nod, neu wag am
  ddim un).
- `-a, --login` — allforio enw mewngofnodi'r sesiwn drwy `NEW-ENVIRON`.
- `-l, --user <name>` — allforio `name` fel yr enw mewngofnodi (yn awgrymu `-a`).
- `-b, --bind <address>` — rhwymo'r cyfeiriad lleol hwn cyn cysylltu.
- `-d, --debug` — olrhain negodi'r dewisiadau ar yr allbwn gwall safonol.
- `-?, --help` — dangos help byr y gorchymyn hwn.

## EXAMPLES

- `telnet example.test` — agor sesiwn ar y porth telnet dynodedig.
- `telnet 10.0.2.2 25` — siarad â llaw â gwasanaeth post.
- `telnet -6 fe80::2` — cysylltu dros IPv6 yn unig.
- `telnet -l ada host` — cynnig `ada` fel yr enw mewngofnodi.
- `telnet -8 host` — gofyn am lwybr 8-did i'r ddau gyfeiriad.
- `telnet` wedyn `open host` — cysylltu o'r anogwr gorchmynion.

## EXIT STATUS

- `0` — bu'r sesiwn (pa bynnag ffordd y daeth y gwesteiwr â hi i ben), neu
  ysgrifennwyd yr help byr.
- `1` — ni fu sesiwn: ni ddatryswyd y gwesteiwr, gwrthodwyd y soced, neu ni
  allai'r derfynell fynd i'r modd crai.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `TERM` — adroddir i'r gwesteiwr drwy'r dewisiad TERMINAL TYPE.
- `USER` — yr enw mewngofnodi y mae `-a` yn ei allforio.
- `LANG` — y locale a ffefrir am yr help byr (tag BCP-47 fel `cy-GB`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
