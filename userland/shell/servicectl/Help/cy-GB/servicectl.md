## NAME

servicectl — cychwyn, stopio, galluogi ac analluogi gwasanaethau system

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

## DESCRIPTION

Yn gofyn i'r rheolwr gwasanaethau newid cyflwr rhedeg gwasanaeth
cofrestredig, drwy ei bwynt terfyn rheoli a warchodir gan allu. Y rheolwr
sy'n penderfynu: dim ond amgodio'r cais ac adrodd yr ateb y mae'r offeryn
hwn yn ei wneud.

Mae cyrraedd y pwynt terfyn ei hun yn awdurdod. Heb
`CAP_SERVICE_CONTROL` yn nenfwd eich cyfrif, mae'r cnewyllyn yn gwrthod yr
alwad cyn i'r rheolwr ei gweld; ni all cyfrif difraint hyd yn oed ofyn.

- `start SERVICE` — codi gwasanaeth cofrestredig sydd i lawr ar hyn o bryd.
  Mae'r amodau parodrwydd y mae'n eu gofyn yn dal i fod: gwrthodir
  gwasanaeth nad yw ei amodau wedi'u bodloni yn lle ei gychwyn i system na
  all ei gynnal.
- `stop SERVICE` — stopio gwasanaeth sy'n rhedeg yn raslon, a'i
  ddibynyddion yn nhrefn gwrthdro'r dibyniaethau. Gofynnir i'r gwasanaeth
  ymadael, a'i orfodi i lawr dim ond ar ôl ei gyfnod gras.
- `enable SERVICE` — cofnodi'r gwasanaeth fel un cofrestredig, fel bod y
  rheolwr yn ei godi ar bob cychwyn, a'i gychwyn nawr.
- `disable SERVICE` — ei gofnodi fel un heb ei gofrestru, fel nad yw unrhyw
  gychwyn diweddarach yn ei godi, a'i stopio nawr.

Ar lwyddiant, mae un llinell yn enwi'r cyflwr y gadawodd y rheolwr y
gwasanaeth ynddo.

Mae'r ddau fath o newid yn effeithio ar bob prifolyn ar y peiriant, nid eich
sesiwn eich hun yn unig. Dim ond y system *sy'n rhedeg* y mae `start` a `stop`
yn ei newid, felly mae gwasanaeth cofrestredig yn dychwelyd wrth gychwyn
nesaf; mae `enable` ac `disable` yn newid y cofrestriad ei hun, ac felly'n
goroesi cychwyn.

## OPTIONS

- `-h, -?` — dangos help byr y gorchymyn hwn a gadael.
- `--` — gorffen yr opsiynau, fel bod modd enwi gwasanaeth y mae ei enw'n
  dechrau â chysylltnod.

## EXIT STATUS

- `0` — cymhwyswyd y weithred, neu dangoswyd yr help byr.
- `1` — gwrthododd y rheolwr y weithred, neu ni allwyd cyrraedd y pwynt
  terfyn rheoli.
- `2` — ni ddeallwyd y llinell orchymyn; ni anfonwyd dim.

## ENVIRONMENT

- `LANG` — yr iaith a ffefrir ar gyfer yr help byr (tag BCP-47 fel `fr-FR`).

## SEE ALSO

- `ps`
- `man`
