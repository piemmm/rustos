## NAME

view — gwyliwr graffigol delweddau a dogfennau

## SYNOPSIS

`view`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n dangos delwedd neu ddogfen. Wedi'i
lansio â dogfen — o'r rheolwr ffeiliau, neu drwy agor delwedd — mae'n dangos
y ffeil honno; wedi'i lansio ar ei ben ei hun, mae'n gofyn i ddewisydd
ffeiliau dibynadwy sesiwn y bwrdd gwaith ddewis un.

Nid oes gan y gwyliwr unrhyw allu ar y system ffeiliau: ni all agor,
rhestru na darllen dim ohono'i hun. Mae'r sesiwn yn pori ar ei ran o dan ei
hunaniaeth ei hun, a dim ond y ffeil y mae'r defnyddiwr yn ei dewis a
ddirprwyir iddo — un tro, ac ar gyfer darllen yn unig. Nid yw'r ffeil byth
yn cael ei dadgodio o fewn y gwyliwr: anfonir ei beitiau i broses waith ar
wahân nad oes ganddi unrhyw gyrhaeddiad at y system ffeiliau, felly ni all
ffeil gam-ffurfiedig na gelyniaethus gyrraedd dim y mae'r gwyliwr yn ei
gyrraedd.

Y fformatau a gynhelir yw JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO a
RISC OS Sprite. Mae ffeil y mae'r dadgodiwr yn ei gwrthod yn nodi ei rheswm
yn y ffenestr ac ar y llif gwallau safonol; ni adewir y ffenestr yn wag
byth, ac ni ffugir delwedd byth.

Mae sawl dogfen ar y tro yn wylwyr ar wahân: cymharu dwy ddelwedd ochr yn
ochr yw agor yr ail. Mae cau ffenestr yn dod â'i gwyliwr i ben.

Mae'r bar offer ar y brig yn cynnwys, yn eu trefn: lleihau, chwyddo, ffitio
yn y ffenestr, maint gwirioneddol, cofnod blaenorol, cofnod nesaf, cylchdroi
i'r chwith, cylchdroi i'r dde, drychu, chwarae neu oedi animeiddiad, a'r
panel gwybodaeth. Mae llithrydd chwyddo di-fwlch ar ei ymyl olaf. Mae'r
llinell statws ar y gwaelod yn nodi enw'r ddogfen, ei fformat, ei maint mewn
picseli, y cofnod a ddangosir, ei hyd, a'r chwyddhad.

Llusgwch y ddelwedd i symud o'i mewn pan fydd yn fwy na'r ffenestr; tra
bydd hi, mae barrau sgrolio'n ymddangos ar ymylon y cynfas. Trowch yr olwyn
dros y ddelwedd i'w symud. Mae gwasgiad eilaidd ar y ddelwedd yn agor
dewislen y gwyliwr, y mae sesiwn y bwrdd gwaith yn ei darlunio.

Dangosir tryloywder ar batrwm gwyddbwyll, fel bod delwedd dryloyw'n darllen
fel un dryloyw ac nid fel y lliw sydd y tu ôl iddi.

* `+` — chwyddo i'r cam nesaf
* `-` — lleihau i'r cam blaenorol
* `0` — ffitio'r ddelwedd gyfan yn y ffenestr
* `1` — maint gwirioneddol, un picsel delwedd i bob picsel sgrin
* `2` — ffitio lled y ddelwedd
* `[` / `]` — chwarter tro i'r chwith neu i'r dde
* `M` — drychu o'r chwith i'r dde
* `I` — dangos neu guddio'r panel gwybodaeth
* `Space` — chwarae neu oedi animeiddiad
* `O` — dewis dogfen arall
* `Page Up` / `Page Down` — y cofnod blaenorol neu nesaf
* `Home` / `End` — y cofnod cyntaf neu olaf
* bysellau saeth — symud o fewn y ddelwedd
* `Escape` — cau'r ffenestr

## OPTIONS

`-h`, `-?`, `--help`
: Ysgrifennu'r cymorth hwn i'r allbwn safonol a therfynu.

## EXIT STATUS

Sero ar ôl cau'n lân. Nid sero pan wrthodwyd sianel y ffenestr, y rhanbarth
ffrâm a rennir, neu sesiwn y bwrdd gwaith; nodir y rheswm ar y llif gwallau
safonol.
