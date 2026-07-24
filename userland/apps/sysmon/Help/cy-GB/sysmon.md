## NAME

sysmon — gwylio cof, storfeydd a llwyth y cnewyllyn yn fyw

## SYNOPSIS

`sysmon [-d eil.degfedau] [-h | -?]`

## DESCRIPTION

Mae `sysmon` yn olygfa fyw, sgrin lawn, o'r hyn y mae'r cnewyllyn yn ei
wneud â chof a'r CPU, wedi'i darllen yn gyfan gwbl trwy'r API Gwybodaeth
System — nid oes `/proc` i'w grafu. Mae'n dangos y cof ffisegol a'i
gyfansoddiad, pentwr y cnewyllyn, y band pwysedd cof a'i hanes diweddar,
cyfriflyfr y storfeydd adenilladwy â **chymarebau taro** fesul dosbarth,
haen gywasgedig `ramzip`, cyfanswm y cof pinnwyd, defnydd storfa'r cyfrolau
sydd wedi'u mowntio, y llwyth fesul CPU, tabl ymyriadau'r cnewyllyn, a
chyfrifiad o brosesau. Mae'n aros yn ddefnyddiadwy tra bo'r system dan
lwyth bwriadol, ac yn gorffwys rhwng adnewyddiadau pan fo'n segur (mae'r
darlleniad yn parcio; nid yw byth yn troelli'n ofer).

Wrth gychwyn, mae'r monitor yn pinio ei gof ei hun (`mem_pin`, sy'n mynnu
`CAP_MEM_PIN`) fel na fydd byth yn glynu ar ei ffaeleddau tudalen ei hun
dan yr union bwysedd y mae'n ei wylio. Adroddir am biniad a wrthodwyd ar y
llinell deitl ac mae'r sesiwn yn parhau heb ei binio — mae'r piniad yn
achlysurol, byth yn angheuol.

Mae'r arddangosfa'n adnewyddu bob cyfwng (3.0 eiliad oni bai bod `-d` yn ei
newid). Nid yw'r monitor yn derbyn operandau: fe'i gyrrir gan fysellau a
wesgir o fewn y sesiwn.

- `q` — gadael.
- Chwith / De (neu `p`) — newid y panel manylion (Chwith = blaenorol, De /
  `p` = nesaf): storfeydd, yr haen gywasgedig, storfa'r cyfrolau sydd wedi'u
  mowntio (disgiau), y llwyth fesul CPU, y llinellau ymyriad, y prosesau.
- `r` — adnewyddu nawr.
- `+` / `-` — ymestyn / byrhau'r cyfwng un eiliad, rhwng 0.1 a 60 eiliad.
- I Fyny/I Lawr, TudalenFyny/TudalenLawr, Cartref/Diwedd — sgrolio'r panel
  ffocysedig.
- `h`, `?` — dangos neu guddio crynodeb bysellau'r sesiwn (sy'n ailgynhyrchu
  allwedd y bariau isod).

### Y bloc crynodeb

Mae bloc crynodeb sefydlog yn rhagflaenu'r panel manylion. Mae pob rhes
wedi'i labelu ar y chwith fel y darllenir hi heb liw; atgyfnerthu yn unig
yw lliw.

- **Llinell deitl** — enw'r offeryn, amser i fyny'r system (`up D days,
  H:MM`), y tair cyfartaledd llwyth (1/5/15 munud), a'r cyflwr pinio
  (`[pinned]`, neu `[unpinned: <reason>]` pan wrthodwyd y piniad).
- **`Mem`** — y bar cof (gweler allwedd y bariau), yna'r ffigurau a
  ddefnyddiwyd / cyfanswm (unedau cryno `K`/`M`/`G`), y ganran a
  ddefnyddiwyd, maint pentwr y cnewyllyn, a — phan na fo'n sero —
  ffigurau'r storfa gywasgedig `ramzip` a'r cof pinnwyd `pinned`. Mae'r bar
  yn crebachu i gadw pob ffigur ar linell 80 colofn, felly ni thorrir
  ffigur byth.
- **`Pres`** — y bar pwysedd cof: mesurydd o bum band, pob band a
  gyrhaeddwyd wedi'i lenwi yn ei liw difrifoldeb ei hun, yna enw'r band
  cyfredol, y ffigurau rhydd / wrth gefn, a chyfanswm y mynediadau i fand.
- **`Hist`** — llain hanes y bandiau pwysedd: un glyff fesul adnewyddiad,
  yr hynaf ar y chwith, pob un wedi'i liwio yn ôl ei fand — `.` normal,
  `-` ysgafn, `=` cymedrol, `#` difrifol, `!` critigol — fel bod cyfnod o
  bwysedd yn darllen fel rhediad lliwiog.
- **`CPU`** — y bar CPU cyfun (gweler allwedd y bariau), yna canran
  prysurdeb pob CPU, nifer y CPU, a chyfrifyddion cyfun y cyfnewidiadau
  cyd-destun a'r rhagflaenau.
- **`Tasks`** — cyfrifiad y prosesau: cyfanswm, yn rhedeg, yn cysgu, wedi'u
  hatal, a sombïaid (ag `(own)` wedi'i ychwanegu pan wrthodwyd cyfrifiad
  pob proses ac mai dim ond tasgau'r galwr ei hun a gyfrifir).
- **Bar tabiau'r paneli** — pob panel manylion, yr un ffocysedig wedi'i
  amlygu, ag arwydd sgrolio ar y dde pan fo'r panel ffocysedig yn gorlifo.

### Allwedd y bariau

Bariau mewn cromfachau sgwâr `[…]` yw'r mesuryddion `Mem` a `CPU`. Mae
crynodeb `?` yn ailgynhyrchu'r allwedd hon o fewn y sesiwn sy'n rhedeg.

Bar **pentyrredig** yw'r bar cof (`Mem`) y mae ei gelloedd yn enwi'r hyn y
mae'r cof ffisegol yn ei ddal — rhaniad *anghysylltiedig* o'r cof a
ddefnyddiwyd (`used` yw `total` llai `free`), fel nad oes dim yn cael ei
gyfrif ddwywaith, a bod y lled a lenwir yn union y ffracsiwn a ddefnyddiwyd:

- `#` — cof preswyl defnyddiwr (gwyrdd): tudalennau preswyl yng ngofodau
  cyfeiriadau defnyddiwr.
- `K` — pentwr y cnewyllyn (gwyrddlas): pentyrrau a slabiau'r cnewyllyn ei
  hun.
- `=` — cof arall mewn defnydd (magenta): popeth a ddefnyddir ond na
  briodolir uchod (storfeydd tudalen, byfferau, fframiau'r cnewyllyn).
- gwag — cof rhydd.

Mae'r storfa gywasgedig `ramzip` a'r cof anhysbys `pinned` yn gorgyffwrdd â'r
bwcedi hynny (tudalennau preswyl defnyddiwr yw tudalennau pinnwyd; cof
cnewyllyn yw'r storfa gywasgedig), felly adroddir amdanynt fel ffigurau
wrth ymyl y bar yn hytrach nag fel segmentau ar wahân a fyddai'n cyfrif
ddwywaith — cyfrifyddu gonest yn hytrach na darlun camarweiniol.

Mae'r bar pwysedd (`Pres`) yn lliwio pob band yn ôl ei ddyfnder:
normal/ysgafn gwyrdd, cymedrol melyn, difrifol/critigol coch.

Mae'r bar CPU (`CPU`) yn llenwi â chelloedd prysur `#` dros drac segur gwag,
wedi'i liwio yn ôl y gyfran brysur (gwyrdd o dan 60 %, melyn o dan 85 %, coch
ar 85 % neu fwy). Mae TAIRiX yn cyfrifyddu amser CPU fel prysur yn erbyn
segur yn unig — nid oes rhaniad defnyddiwr/system/mewnbwn-allbwn yn yr API —
felly mae'r bar yn dangos un categori prysurdeb gonest, gyda'r manylder
fesul craidd yn y panel `cpu`.

### Y paneli manylion

Mae Chwith / De (neu `p`) yn tramwyo chwe phanel. Mae gan bob un bennawd
colofn gwrthdro (fideo gwrthdro, trwm) fel bod y pennawd yn darllen fel bar
ar wahân uwchben y corff.

### caches — cyfriflyfr y storfeydd adenilladwy

Dyma'r storfeydd y gall y cnewyllyn eu dychwelyd i leddfu pwysedd cof **heb
golli data**: mae pob cofnod yn ailadeiladadwy o'i ffynhonnell ganonaidd,
felly mae'r cnewyllyn yn ei ollwng yn hytrach na'i dudalennu allan. Y panel
yw'r ateb uniongyrchol i "a yw'r storfeydd yn gwneud eu gwaith?": mae pob
rhes yn un dosbarth adennill, wedi'i gyfanredu ar draws pob storfa
gofrestredig, ac yn cario ei **gymhareb taro** ei hun.

Colofnau:

- `class` — y dosbarth adennill (gweler y rhestr ddosbarthiadau isod).
- `entries` — cofnodion byw a ddelir ar hyn o bryd i'r dosbarth.
- `cached` — ôl troed preswyl y dosbarth: llwyth defnyddiol y cofnodion
  ynghyd â metadata cyfrifyddu fesul cofnod, gyda'i gilydd.
- `hits` — chwiliadau'r dosbarth a wasanaethwyd o'r storfa ers cychwyn
  (osgôdd y storfa'r ffynhonnell ganonaidd).
- `misses` — chwiliadau'r dosbarth a syrthiodd trwodd i'r ffynhonnell
  ganonaidd ers cychwyn.
- `hit%` — cymhareb effeithiolrwydd y storfa, `hits / (hits + misses)` fel
  canran gyfan. Mae cymhareb uchel yn golygu bod y storfa'n ennill ei chof;
  mae un isel yn golygu ei bod yn dal cof heb osgoi gwaith. Mae'n darllen
  `-`, byth `0%` ffug, ar gyfer dosbarth nad yw dim wedi'i chwilio y
  cychwyniad hwn (enwadur segur).
- `ref` — derbyniadau a **wrthodwyd** ers cychwyn (cofnod y gwrthododd y
  storfa ei ddal: dros y gyllideb, anghyfrifadwy, neu heb gof).
- `shr` — teithiau **crebachu** a orfodwyd gan bwysedd a adenillodd
  gofnodion o'r dosbarth ers cychwyn.
- `fail` — **methiannau** mewnol a briodolir i'r dosbarth: nam cyfriflyfr a
  ganfuwyd a wenwynodd (analluogi'n fail-closed) storfa.

Byrheir cyfrifon uwchlaw 99 999 fel `k`/`M`/`G`/`T` (miloedd degol, nid KiB)
fel na fydd colofn byth yn lledu.

Y dosbarthiadau adennill, yn nhrefn adennill y cnewyllyn dan bwysedd (mae'r
cyntaf a restrir yn cael ei ollwng gyntaf, felly mae storfa isel yn y
rhestr yn goroesi hwyaf):

- `disposable-ui` — cyflwr rhyngwyneb gwaredadwy (asedau rasterig, atlasau
  glyffiau, ciplun ffenestri): rhataf i'w golli, cyntaf i fynd.
- `predictive-prefetch` — data a ragnôl yn ddyfaliadol (rhestrau, mân-luniau,
  mynegeion cwblhau): byth yn angenrheidiol ar gyfer cywirdeb.
- `background-validation` — cynhyrchion gwaith dilysu amser segur (cynnydd
  sganio, olion bysedd ymgeisiol): mae'r gwaith dyfaliadol yn peidio cyn
  gynted ag y dechreua'r pwysedd.
- `semantic-app-cache` — cyflwr lansio ap wedi'i ddilysu (maniffestau
  dosrannedig, crynodebau dilysu, canlyniadau datrys gorchmynion). Ni all ei
  adennill byth wneud ap yn anlansadwy — dim ond ailredeg a wna'r glwyd
  lwytho.
- `runtime-cache` — cyflwr deilliedig sy'n eiddo i'r amser rhedeg (paratoad
  y llwythwr, mapiau adnoddau): grwpiwyd gyda'r storfa semantig.
- `clean-file-data` — *cynnwys* ffeil glân, ailadeiladadwy, y gellir ei
  ailddarllen o'r gyfrol: mae un darlleniad dyfais ffiniedig yn ailadeiladu
  talp. Adennillir cyn cywasgu dim i mewn i `ramzip`.
- `transform-cache` — ffurfiau canolradd drud o ddata awdurdodedig (data
  clwstwr wedi'i ddilysu, ei ddadgryptio, ei ddadgywasgu): drutach i'w
  ailadeiladu na darlleniad glân, felly adennillir ar ôl y data ffeil glân.
- `fs-metadata` — metadata'r system ffeiliau: cofnodion stat, canlyniadau
  chwilio enwau, cofnodion cyfeiriaduron, a chofnodion diogelwch. Bach, poeth,
  ac wedi'u hailadeiladu gan dramwyad coeden aml-gam yn unig, felly maent yn
  goroesi'r data ffeil dan bwysedd.
- `reliability-assist` — cyflwr cymorth adfer ailadeiladadwy (ffenestri
  gwirio, crynodebau iechyd): cyfiawnhawyd gan hwyrni adfer, felly cedwir
  hwyaf.

### ramzip — yr haen gof gywasgedig

Mae `ramzip` yn cywasgu tudalennau anhysbys oer i mewn i storfa lai yn y RAM
yn hytrach na'u tudalennu allan. Ei adrannau:

- `tier` — yr ôl troed byw: `entries` a ddelir, beit `logical` (heb eu
  cywasgu) a gynrychiolir, beit `stored` (testun cêl) a ddelir mewn
  gwirionedd, a beit `metadata` cyfrifyddu; yna `saved` (rhesymegol llai
  storiedig) â'i ganran o'r rhesymegol — y cof y mae'r haen yn ei ennill yn
  ôl.
- `capacity` — y capiau deilliedig y mae'r haen yn ei feintio ei hun atynt:
  `min` (ar gael bob amser), `soft` (targed), `hard` (nenfwd), a'r beit
  `pinned` cyfredol.
- `compress` — y llwybr storio (ysgrifennu): `attempts` a gynigiwyd,
  `accepted` a storiwyd, a'r **gyfradd dderbyn** (a dderbyniwyd / ymdrechion)
  — cymhareb daro'r haen hon ei hun ar gyfer cywasgu. Oddi tanodd, y
  dadansoddiad gwrthod: anghywasgadwy, polisi, cap, anghymwys, wrth gefn,
  cyfran tasg, a gwrthodiadau dyrnu.
- `restore` — y llwybr adfer (darllen): `faults` tudalen, adferiadau `warm`,
  adferiadau `clustered`, a'u cyfanswm `restored`; yna'r `failures`
  (dilysu / dadgodio) a'r **gyfradd lwyddiant** (a adferwyd / (a adferwyd +
  methiannau)). Mae pob cymhareb yn ganran, neu'n `-` ar gyfer enwadur segur.
- `warm-up` — `attempts` yr adferwr cynnes cefndir, ei gyfrif `stopped`, a'i
  gyfrif `thrash-detected`.

### disks — storfa'r cyfrolau sydd wedi'u mowntio

Un rhes arddull `df` fesul cyfrol wedi'i mowntio: pwynt mowntio, math system
ffeiliau, maint cyfan, a ddefnyddiwyd, ar gael, canran defnydd, a bar
defnydd ASCII. Mae cyfrol nad yw ei gyrrwr yn adrodd unrhyw gapasiti yn
dangos `capacity unknown` yn hytrach na maint ffug; tynnir cyfrol a
dynnwyd yn annisgwyl neu mewn gwrthdaro adfer yn y cyflwyniad rhybudd ac fe'i
nodir (`[unavailable-dirty]`, `[unavailable-lost]`, `[recovery-conflict]`).
Nid oes cyfrifyddion trwybwn mewnbwn-allbwn fesul dyfais yn yr API, felly
capasiti a defnydd gonest yw hyn, nid cyfraddau trosglwyddo ffug.

### cpu — llwyth fesul CPU

Un rhes fesul CPU: ei chyfran brysur dros y cyfwng (`busy%`), dyfnder ei
chiw rhedeg (`queue`), a'i chyfrifon o gyfnewidiadau cyd-destun (`switches`)
a rhagflaenau (`preemptions`) ers cychwyn.

### irqs — llinellau ymyriad

Un rhes fesul llinell ymyriad rwymedig, yn nhrefn esgynnol llinellau: id y
llinell, y dasg gyrrwr sy'n berchen (`owner`), y `count` ymyriadau ers
cychwyn, a `state` y llinell — `active`, neu `quarantined` (wedi'i thynnu yn
y cyflwyniad rhybudd) pan fo rhwyd ddiogelwch y cnewyllyn yn erbyn
llinellau afreolus wedi'i analluogi.

### procs — cyfrifiad y prosesau

Y defnyddwyr mwyaf yn ôl `%cpu` ac yn ôl cof (`size`), pob un â'i pid, ei
orchymyn, a — ar gyfer y tabl cof — ei gyflwr. Gwaith `top` yw'r rhestr
brosesau ryngweithiol lawn; dim ond crynodeb y cyfrifiad yw hwn.

### Galluoedd

Mae pob ffigwr yn teithio trwy'r API Gwybodaeth System. Mae ymholiadau
ystadegau led-cnewyllyn (cof, pwysedd, storfeydd, `ramzip`, llwyth fesul
CPU) yn mynnu `CAP_SYSINFO_KERNEL`; mae panel y llinellau ymyriad yn mynnu
`CAP_SYSINFO_HW`; mae cyfrifiad pob proses yn mynnu `CAP_SYSINFO_GLOBAL`.
Mae galwr heb un yn gweld gwrthodiad y panel hwnnw wedi'i egluro — byth
ffigwr ffug — tra bo gweddill y sesiwn yn parhau (methu ar gau, dirywio'n
raslon). Nid yw storfa'r cyfrolau sydd wedi'u mowntio wedi'i chyfyngu.

## OPTIONS

- `-d, --delay <seconds>` — y cyfwng rhwng adnewyddiadau awtomatig, mewn
  eiliadau â ffracsiwn dewisol (dim ond y digid degol cyntaf, y degfedau, a
  gedwir): mae `sysmon -d 1.5` yn adnewyddu bob 1.5 eiliad. Rhagosodiad 3.0.
  Mae GNU `top` yn derbyn cyfwng sero ac yn adnewyddu mor gyflym ag y gall;
  nid yw TAIRiX byth yn troelli'n ofer, felly codir sero i'r isafswm o 0.1 s.
- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael. O fewn
  sesiwn sy'n rhedeg, mae'r un bysellau'n toglo crynodeb y bysellau yn lle
  hynny.

## EXIT STATUS

- `0` — daeth y sesiwn i ben â `q`, neu dangoswyd y cymorth byr.
- `1` — methodd y derfynell; ysgrifennir y rheswm i'r allbwn gwall.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale ffafredig ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
