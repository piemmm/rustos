## NAME

terminal — efelychydd terfynell graffigol

## SYNOPSIS

`terminal`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n cynnal cragen ragosodedig y
defnyddiwr ar sgrin 80×25 nod. Anfonir bysellau a deipir i'r ffenestr
â ffocws at y gragen; dehonglir popeth y mae'r gragen yn ei ysgrifennu
(yr allbwn safonol a'r gwall safonol fel ei gilydd) drwy'r eirfa
ANSI/VT a rennir a'i dynnu yn y cynllun lliw a ddewiswyd yn y
gosodiadau. Nid yw'r derfynell ei hun byth yn adleisio: mae adlais a
golygu llinell yn perthyn i'r gragen, yn union fel ar gonsol.

Mae'r ffenestr yn agor yn beth bynnag y mae'r sgrin 80×25 yn ei fesur
yn y maint testun sydd mewn grym, felly mae'n ffitio'r arddangosiad y
dangosir ef arno; ar sgrin sy'n rhy fach ar gyfer y maint hwnnw, mae'r
testun yn cael ei gamu i lawr yn hytrach na chyfyngu'r sgrin, oherwydd
rhaid i raglen sy'n gosod ei hun ar gyfer 80 colofn eu cael o hyd.

Lansir y derfynell o Lyfrgell Raglenni'r bwrdd gwaith (botwm `Library` y
bar tasgau) neu wrth ei henw o gragen. Mae angen sesiwn graffigol
weithredol arni: hebddi, mae sianel y ffenestr yn anghyraeddadwy ac
mae'r derfynell yn adrodd y gwrthodiad ar y ffrwd gwall safonol ac yn
gorffen.

Daw'r sesiwn i ben pan fydd y gragen yn gadael (er enghraifft gydag
`exit`) neu pan gaeir y ffenestr o'r bwrdd gwaith; mae cau'r ffenestr
yn gorffen y gragen gyda diwedd ffeil ar ei mewnbwn.

Mae gwasgu ail fotwm (de) y llygoden unrhyw le ar y sgrin yn agor
dewislen y derfynell. Mae gan bob rhes lwybr byr bysellfwrdd sy'n
gweithio p'un a yw'r ddewislen yn agored ai peidio, ac mae `Escape` —
neu glicio i ffwrdd o'r ddewislen — yn ei diswyddo heb ddewis.

| Rhes | Llwybr byr | Beth mae'n ei wneud |
| --- | --- | --- |
| Gosodiadau… | `Ctrl ,` | Agor y gosodiadau a ddisgrifir isod. |
| Testun mwy | `Ctrl +` | Tynnu'r sgrin un cam yn fwy. |
| Testun llai | `Ctrl -` | Tynnu'r sgrin un cam yn llai. |
| Maint gwirioneddol | `Ctrl 0` | Dychwelyd i'r maint testun rhagosodedig. |
| Clirio'r sgrin | `Ctrl Shift K` | Gwagio'r sgrin heb ysgrifennu at y gragen. |
| Cau | `Ctrl Shift W` | Cau'r ffenestr a gorffen y gragen. |

Mae'r gosodiadau'n agor yn y ffenestr ei hun ac mae ganddynt ddau dab.
**Ymddangosiad** sy'n dewis y cynllun lliw, yn gosod maint y testun, ac
yn golygu cynllun y defnyddiwr ei hun. Y cynlluniau a gludir yw *System*
(sy'n dilyn ymddangosiad tywyll neu olau'r bwrdd gwaith), *Midnight*,
*Phosphor*, *Amber*, *Ember*, *Contrast*, *Paper*, a *Custom*. Mae dewis
*Custom* yn defnyddio'r lliwiau a olygwyd o dan y dewisydd: grid o'r
ugain lliw y tynnir sgrin ohonynt — y cefndir, y blaendir, y cyrchwr,
testun y cyrchwr, a'r un ar bymtheg o liwiau ANSI — gyda llithryddion
coch, gwyrdd a glas ar gyfer pa un bynnag sydd wedi'i ddewis.

**Effeithiau** sy'n gosod sut mae'r sgrin yn cael ei thynnu.

| Effaith | Beth mae'n ei wneud |
| --- | --- |
| Didreiddedd | Pa mor solet yw'r cefndir. O dan y llawn, mae'r bwrdd gwaith yn dangos drwodd y tu ôl i'r testun, sy'n aros yn gwbl ddarllenadwy. |
| Niwlio'r cefndir | Pa mor bell y mae'r bwrdd gwaith y tu ôl i ffenestr dryloyw yn cael ei niwlio. Nid yw'n cael unrhyw effaith ar ffenestr sy'n gwbl anhydraidd. |
| Llinellau sganio | Pylu rhesi bob yn ail, rhan wastad edrychiad mwgwd cysgodol. |
| Llewyrch | Taenu golau picseli llachar i'w cwmpas, fel bod testun yn cario'r eurgylch meddal sydd gan diwb a yrrir yn galed. |
| Sŵn | Llawr sŵn symudol fesul picsel, fel y mae gan signal analog. |
| Ffosffor | Pa mor hir y mae picsel wedi'u goleuo yn parhau, fel bod testun sy'n sgrolio'n gyflym yn gadael llwybr. |
| Siglo | Siglo llorweddol araf sy'n teithio, fel y mae gan diwb sydd allan o amser. |

Mae pob newid yn dod i rym ar unwaith ac yn cael ei gadw i broffil y
defnyddiwr ei hun, fel bod terfynell ddiweddarach yn agor yr un ffordd.
Mae'r system weithredu'n cadw'r proffil trwy ei gwasanaeth gosodiadau, ac
mae'n breifat i'r derfynell: ni all unrhyw raglen arall ei ddarllen na'i
newid. Dim ond yr hyn a newidiodd y defnyddiwr mewn gwirionedd a gedwir,
felly mae *Adfer rhagosodiadau* yn tynnu'r dewisiadau hynny yn lle rhewi
gwerthoedd heddiw — yna mae gosodiad y mae'r gweinyddwr neu fersiwn
ddiweddarach o'r derfynell yn ei newid yn berthnasol. Gedwir gosodiad na
all y derfynell wneud synnwyr ohono ar ei ragosodiad ac adroddir amdano ar
y ffrwd gwall safonol, ac mae gwasanaeth gosodiadau na ellir ei gyrraedd yn
gadael y derfynell yn rhedeg ar y gwerthoedd y daw gyda hi, ac adroddir am
hynny hefyd.

## EXIT STATUS

Sero ar ôl cau glân neu ymadawiad y gragen ei hun; heb fod yn sero pan
na ellid cynnal y gragen neu pan wrthodwyd sianel y ffenestr, y rhanbarth
fframiau a rennir neu'r blwch digwyddiadau (nodir y rheswm ar y ffrwd
gwall safonol).

## ENVIRONMENT

`HOME`
: Cyfeiriadur cartref y cyfrif, lle mae'r derfynell yn darllen ac yn
ysgrifennu ei phroffil. Hebddo, mae'r derfynell yn rhedeg ar y proffil
rhagosodedig ac nid yw'n cadw dim.

`TERM`
: Fe'i hallforir i'r gragen a gynhelir fel `xterm-256color`, gan enwi'r
efelychydd y mae'r derfynell hon yn ei gyflwyno. Disodlir unrhyw werth a
etifeddwyd; anfonir gweddill yr amgylchedd ymlaen at y gragen yn
ddigyfnewid.

## SEE ALSO

`elsh`, `sysinfo`
