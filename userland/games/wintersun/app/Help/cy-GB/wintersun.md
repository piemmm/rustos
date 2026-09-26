## NAME

wintersun — cerdded byd gaeafol a gynhyrchir yn weithdrefnol

## SYNOPSIS

`wintersun [--reference-scene]`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith ar fyd a gynhyrchwyd: golwg o'r awyr ar dir y
mae'r peiriant yn ei syntheseiddio yn hytrach na'i gludo, wedi'i oleuo gan
haul isel sy'n taflu cysgodion hir i lawr pob llethr.

Nid oes dim o'r byd hwn wedi'i storio fel gwaith celf. Mae pob defnydd y mae'r
tir wedi'i wneud ohono — eira, craig, graean, rhostir, twndra — yn llond dwrn
o rifau y mae'r cleient yn eu troi'n wead wrth ddarlunio, felly mae'r byd yr
un fath ar bob peiriant ac nid yw'n cymryd fawr ddim lle ar ddisg. Mae ffyrdd
yn treulio i mewn i'r hyn y maent yn ei groesi yn hytrach nag eistedd ar ei
ben.

Mae tir nad yw wedi'i gynhyrchu eto yn cael ei ddarlunio fel y bwlch ydyw, ac
yn llenwi wrth iddo gyrraedd. Mae'r cleient yn darlunio'r hyn sydd ganddo yn
hytrach na stopio i aros, felly mae'r ffenestr yn dal i ateb tra bo'r byd yn
dal i fyny.

Mae'r bysellau saeth neu `W`, `A`, `S`, `D` yn cerdded. Mae dwy wedi'u dal
gyda'i gilydd yn cerdded y groeslin rhyngddynt ar yr un cyflymder, ac mae
bysellau gwrthgyferbyniol yn canslo. Mae'r olygfa'n eich dilyn ac yn stopio ar
ymyl y byd yn hytrach na llithro oddi arno. Mae llethrau rhy serth i'w dringo
a dŵr rhy ddwfn i'w rydio yn eich troi o'r neilltu.

Mae `+` a `-` yn symud yr olygfa'n nes ac ymhellach, trwy bum cam rhwng un
gell byd ar draws wyth picsel ac un ar draws cant dau ddeg wyth.

Mae `F11` yn rhoi'r ffenestr yn sgrin lawn ac yn ei dychwelyd i beth bynnag
oedd hi o'r blaen, felly mae ffenestr a chwyddwyd yn dod yn ôl wedi'i chwyddo.
Mae `Esc` yn ei hadfer. Mae `Q` yn gadael.

Mae'r cleient yn darlunio i gyllideb ffrâm. Pan na all gyrraedd un, mae'n
gollwng manylder mewn trefn sefydlog — dwysedd gronynnau, yna cydraniad y
byffer golau, yna manylder y defnyddiau, yna'r cysgodion, yna'r maint y mae'n
ei rendro — ac nid y gyfradd fframiau byth yw'r hyn sy'n ildio. Rhoddir pob
cam yn ôl unwaith y bu'r fframiau'n gyfforddus am gyfnod. Mae'r drefn yn
sefydlog fel bod yr hyn a welwch ar beiriant araf yn rhagweladwy yn hytrach na
syndod.

Mae ffenestr sy'n fwy nag y gall y rendrwr meddalwedd ei llenwi yn cael ei
darlunio ar hyd at 2560×1440 ac yna ei graddio i fyny i'r ffenestr.

## OPTIONS

- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn.
- `--reference-scene` — lluniadu'r olygfa gyfeirio sefydlog a'i chadw'n
  llonydd: un byd, yr un cymeriadau a'r un eiliad, yr un fath ar bob peiriant,
  fel y gellir cymharu llun o'r ffenestr ag un a luniwyd yn rhywle arall. Mae
  `F11` ac `Esc` yn dal i newid maint y ffenestr; nid oes dim arall yn symud.

## EXIT STATUS

`0` pan fyddwch yn gadael. Mae statws nad yw'n sero yn enwi ei reswm ar yr
allbwn gwall safonol: ni ellid cynhyrchu'r byd, ni ellid agor y ffenestr, neu
collwyd sianel digwyddiadau'r sesiwn.

- `2` — ni ddeallwyd y llinell orchymyn.
- `87` — ni ellid lluniadu'r olygfa gyfeirio.

## SEE ALSO

`sapper`
