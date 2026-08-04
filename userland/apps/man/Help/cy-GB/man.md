## NAME

man — dangos dogfen gymorth gorchymyn

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Mae'n dangos y ddogfen gymorth y mae bwndel rhaglen gorchymyn yn ei
chynnwys, yn eich iaith chi pan fo cyfieithiad ar gael.

Mae pob rhaglen TAIRiX yn fwndel rhaglen sy'n cario coeden `Help/`: un
ddogfen strwythuredig i bob gorchymyn neu bwnc, i bob iaith. Mae `man` yn
datrys `<command>` yn union fel y gragen — y rhagddodiad sefydlog o
storfeydd `/System/Commands`, `/System/Applications`, `<home>/Commands` a
`<home>/Applications` yn gyntaf, yna'r cyfeiriaduron ar `PATH` — felly
mae'r dudalen a ddangosir bob amser yn disgrifio'r rhaglen y byddai'r
gragen yn ei rhedeg am yr un gair; ni all `PATH` ei aildrefnu na'i
ddisodli. Mae ôl-ddodiad `.app` yn enwi'r bwndel yn uniongyrchol. Pan nad
yw'r un o'r rhain yn cynnwys y gair, mae `man` yn chwilio'r storfeydd
rhaglenni yn ailadroddus — `/Apps` yn gyntaf, yna'r ffolderi `Commands`
ac `Applications` yn eich cartref — felly mae bwndel a gadwyd mewn
ffolderi nythog yn dal i gael ei ganfod; nid yw'r chwilio byth yn edrych
y tu mewn i fwndel arall, a'r cydweddiad basaf sy'n ennill.

Dewisir y ddogfen yn ôl y locale yn y newidyn amgylchedd `LANG`, gan
gwympo'n ôl i'r un iaith mewn rhanbarth arall ac yn olaf i'r ddogfen
Saesneg ganonaidd. Pan na ddangosir y dudalen yn yr iaith a ofynnwyd
amdani, mae `man` yn nodi'r cyfnewid ar y ffrwd gynghorol (fd 3); nid
yw'r dudalen ei hun byth yn cymysgu ieithoedd.

Ar gonsol rhyngweithiol dangosir y dudalen sgrinaid ar y tro: mae'r
bylchwr yn troi'r dudalen, mae enter yn symud un llinell ymlaen ac mae
`q` yn stopio. Pan ailgyfeirir yr allbwn neu pan fo maint y consol yn
anhysbys, ffrydir y dudalen gyfan.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `man ps` — dangos tudalen `ps`.
- `man top keys` — dangos y pwnc `keys` o fwndel `top`.
- `man files.app` — enwi'r bwndel yn uniongyrchol.

## EXIT STATUS

- `0` — dangoswyd y dudalen.
- `1` — ni chanfuwyd y gorchymyn na'i ddogfen gymorth, neu ni ellid
  cyflwyno'r dudalen.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir (tag BCP-47 fel `cy-GB`).
- `PATH` — y cyfeiriaduron ychwanegol i chwilio am fwndeli
  `<command>.app`, ar ôl y rhagddodiad sefydlog o storfeydd.
- `HOME` — yn enwi eich ffolderi `Commands` ac `Applications` eich hun ar
  gyfer y chwilio ailadroddus am fwndeli.

## SEE ALSO

- `elsh`
