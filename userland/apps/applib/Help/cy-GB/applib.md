## NAME

applib — gweinyddu llyfrgell raglenni'r bwrdd gwaith

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Gweinyddu'r llyfrgell raglenni — y catalog wedi'i drefnu mewn ffolderi
o raglenni y gellir eu lansio y mae lansydd y bwrdd gwaith yn ei
gyflwyno. Data ar y cyfrol yw'r llyfrgell, nid rhestr wedi'i
hymgorffori: stôr ar draws y peiriant yn
`/System/Settings/ProgramLibrary/library.conf` y mae pob cyfrif yn ei
darllen, ynghyd â throslun dewisol fesul defnyddiwr yn yr un llwybr y tu
mewn i `Settings/` y defnyddiwr ei hun. Yr hyn y mae lansydd yn ei
ddangos yw'r ddau wedi'u datrys gyda'i gilydd: mae cofnodion ac
addasiadau'r defnyddiwr ei hun yn trechu'r rhai ar draws y peiriant.

Heb is-orchymyn (neu gyda `list`), caiff y llyfrgell ddatrys ei hargraffu
ffolder wrth ffolder, un cofnod fesul llinell: dynodwr, enw arddangos, a
llwybr y bwndel — yn union yr hyn y mae'r lansydd yn ei ddangos. Y
ffolderi yw'r set gaeedig `Accessories`, `Graphics`, `Internet`,
`Multimedia`, `Office`, `Programming`, `Games`, `SystemTools`,
`Utilities`, ac `Other`; nid oes ffolder ffurf rydd.

Mae `applib add` yn cofrestru bwndel rhaglen. Cymerir ei hunaniaeth, enw
arddangos, ffolder, ac eicon o faniffest `AppInfo` wedi'i lofnodi'r
bwndel ei hun; mae `--category`, `--name`, a `--icon` yn disodli'r
maniffest. Mae angen `--category` penodol ar fwndel nad yw ei faniffest
yn datgan ffolder llyfrgell — nid yw'r teclyn byth yn dyfalu. Mae
`applib remove` yn gollwng cofnod, a enwir wrth ei ddynodwr neu wrth y
llwybr bwndel y cafodd ei gofrestru ag ef.

Mae `applib hide` yn atal cofnod o'r llyfrgell ddatrys heb ddileu ei
gofnod — mae ei ddynodwr yn aros wedi'i hawlio, fel na all `rescan`
diweddarach ei atgyfodi — ac mae `applib show` yn ei ddangos eto. Mae
cuddio yn fater o gyflwyniad, nid awdurdod byth: mae lansio bwndel yn
dal i gael ei reoli gan wiriadau llofnod a galluogrwydd (capability) y
llwythwr waeth beth fo'r catalog.

Mae `applib rescan` yn cerdded y storfeydd rhaglenni (`/System/Commands`,
`/System/Applications` ac `/Apps`, neu `<home>/Commands` a
`<home>/Applications` y defnyddiwr ei hun o dan `--user`), yn darllen
maniffest pob bwndel, ac yn cofrestru pob rhaglen sy'n gofyn am gael ei
rhestru ac nad yw wedi'i chatalogio eto. Ni amharir byth ar gofnodion
presennol — gan gynnwys ail-enwi ac ataliadau curadur — ac mae bwndel
gyda maniffest sy'n amhosibl ei ddarllen neu sydd wedi'i gamffurfio yn
cael ei hepgor a'i gyfrif, nid yw byth yn rheswm dros erthylu. Dyma sut
mae llyfrgell system newydd yn ymgartrefu o'r bwndeli sydd wedi'u
gosod mewn gwirionedd, heb unrhyw restr wedi'i chynnal â llaw yn unman.

Yn ddiofyn mae'r teclyn yn golygu'r stôr ar draws y peiriant, y gall dim
ond pennaeth a dderbynnir gan bolisi ysgrifennu `/System/Settings` ei
newid; mae cyfrif cyffredin yn ei ddarllen ond yn ei bersonoli trwy ei
droslun ei hun gyda `--user`. Mae ysgrifennu a wrthodir yn nodi ei reswm
ac nid yw'n newid dim.

Ar lwyddiant mae'r teclyn yn dawel ar allbwn safonol; caiff canlyniad
newid ei ryddhau fel cofnod ymgynghorol strwythuredig ar y ffrwd wybodaeth
safonol (fd 3), y gall sgriptiau ei ddal gyda `3>records.jsonl` a gall
popeth arall ei anwybyddu.

## OPTIONS

- `--category <folder>` — gyda `list`, dangos y ffolder honno yn unig;
  gyda `add`, ffeilio'r cofnod oddi tani (gan ddisodli datganiad y
  maniffest).
- `--name <name>` — gyda `add`, yr enw arddangos i'w ddangos yn lle'r
  un yn y maniffest.
- `--icon <asset>` — gyda `add`, yr ased eicon (enw ffeil y tu mewn i
  `Resources/` y bwndel) yn lle'r un yn y maniffest.
- `--user` — cymhwyso'r newid i droslun y defnyddiwr ei hun (neu, gyda
  `rescan`, cerdded `<home>/Commands` a `<home>/Applications` y
  defnyddiwr ei hun) yn lle'r stôr ar draws y peiriant.
- `-h, -?` — dangos help byr y gorchymyn hwn.

## EXAMPLES

- `applib` — dangos y llyfrgell ddatrys, ffolder wrth ffolder.
- `applib list --category Games` — dangos un ffolder.
- `applib add /Apps/chess.app` — cofrestru bwndel fel y mae ei faniffest
  yn gofyn.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  cofrestru bwndel nad yw'n datgan rhestriad, o dan ffolder benodol.
- `applib remove os.tairix.chess` — gollwng cofnod wrth ei ddynodwr.
- `applib hide os.tairix.chess --user` — ei guddio o'ch llyfrgell eich
  hun yn unig.
- `applib rescan` — cofrestru pob bwndel wedi'i osod sydd wedi'i restru
  ac nad yw eto yn y catalog peiriant.

## EXIT STATUS

- `0` — cwblhawyd y rhestru, y newid, y rescan, neu'r help byr.
- `1` — methiant stôr, bwndel, neu allbwn (er enghraifft efallai na fydd
  y defnyddiwr yn cael newid y catalog ar draws y peiriant); nodir y
  rheswm ar y ffrwd ddiagnostig.
- `2` — ni ddeallwyd y llinell orchymyn, mae'r ffolder neu'r cofnod yn
  anhysbys, neu ni ellir cofrestru'r bwndel fel y gofynnwyd.

## ENVIRONMENT

- `LANG` — y locale dewisol ar gyfer yr help byr (tag BCP-47 fel `fr-FR`).
- `HOME` — cyfeiriadur cartref y defnyddiwr: yn enwi'r troslun fesul
  defnyddiwr a gwreiddiau'r rescan `--user` `<home>/Commands` a
  `<home>/Applications`.

## SEE ALSO

- `man`
- `configure`
