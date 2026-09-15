## NAME

sapper — clirio'r maes mwynglawdd heb daro ffrwydryn

## SYNOPSIS

`sapper`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n dal grid o gelloedd wedi'u gorchuddio. Mae
rhai yn cuddio ffrwydryn. Mae pob cell a ddatgelir nad yw'n cuddio un yn dangos
faint o'i wyth cymydog sy'n gwneud hynny, ac mae'r rhifau hynny'n ddigon i
weithio allan lle mae'r ffrwydron. Datgelwch bob cell nad yw'n ffrwydryn ac
rydych wedi ennill; datgelwch un sydd, ac mae'r gêm ar ben.

Mae'r gell gyntaf a ddatgelwch bob amser yn ddiogel, ac felly hefyd yr wyth o'i
chwmpas, felly ni all symudiad agoriadol byth golli ac mae bob amser yn agor
ardal i resymu ohoni.

Cliciwch gell wedi'i gorchuddio i'w datgelu. Cliciwch hi â'r botwm eilaidd i
blannu baner, eto am nod cwestiwn os yw'r rheini ymlaen, ac eto i'w chlirio. Mae
cell â baner wedi'i diogelu: nid yw clicio arni'n gwneud dim.

Unwaith y bydd cell ar agor, gall ei rhif wneud y gwaith drosoch. Cliciwch rif
agored y mae ei faneri eisoes yn cyfateb iddo ac mae pob cymydog sy'n weddill yn
cael ei ddatgelu ar unwaith. Cliciwch ef â'r botwm eilaidd pan fo ei gymdogion
gorchuddiedig yn union gymaint â'i rif ac fe'u baneri i gyd ar unwaith. Mae'r
botwm canol yn gwneud yr un peth â'r cyntaf, o unrhyw le ar y gell.

Mae'r cownter ar y chwith yn dangos faint o ffrwydron sydd ar ôl i'w canfod, llai
y baneri a osodwyd gennych; mae'n mynd yn negatif os gosodwch fwy o faneri nag y
mae o ffrwydron. Mae'r cloc ar y dde yn cychwyn ar eich cell gyntaf a ddatgelwyd
ac yn stopio pan ddaw'r gêm i ben. Mae'r botwm rhyngddynt yn cychwyn gêm newydd,
ac mae ei wyneb yn dweud sut yr aeth yr un bresennol.

Mae'r bysellau saeth yn symud cylch o amgylch y bwrdd. Mae `Space` yn datgelu'r
gell y tu mewn iddo, neu'n ei chordio pan fydd eisoes ar agor. Mae `F` yn nodi'r
gell, `Shift+F` yn baneru ei holl gymdogion gorchuddiedig, `N` yn cychwyn gêm
newydd, ac mae `1`, `2` a `3` yn dewis y byrddau dechreuwr, canolradd ac arbenigwr.
Mae'r un dewisiadau, a'r gosodiad nod cwestiwn, ar ddewislen bar eiconau'r gêm.

Cofir eich amser gorau ar bob un o'r tri bwrdd safonol rhwng sesiynau. Nid yw
bwrdd o'ch maint eich hun yn cadw'r un, oherwydd nid yw dau fwrdd o'r fath byth
yr un gêm.

Lansir y gêm o Lyfrgell Rhaglenni'r bwrdd gwaith, o dan Games, neu wrth ei henw o
gragen. Mae angen sesiwn graffigol sy'n rhedeg: hebddi mae sianel y ffenestr yn
anghyraeddadwy ac mae'r gêm yn adrodd y gwrthodiad ar y ffrwd gwallau safonol ac
yn gadael.

## EXIT STATUS

Sero ar ôl cau'n lân; heb fod yn sero pan wrthodwyd sianel y ffenestr neu'r
rhanbarth ffrâm a rennir (nodir y rheswm ar y ffrwd gwallau safonol).
