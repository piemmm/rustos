## NAME

mdadm — archwilio a gweinyddu araeau RAID

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Yn archwilio ac yn gweinyddu'r araeau RAID meddalwedd y mae'r cyfansoddwr
araeau yn eu cydosod o ddyfeisiau aelod. Darllenir rhestr yr araeau a'r
dyfeisiau drwy'r API Gwybodaeth System — yr un rhyngwyneb, ar yr un lefel
`CAP_SYSINFO_HW` ag y darllenir y goeden galedwedd oddi tani. Anfonir y
gweithredoedd creu, ychwanegu, tynnu a stopio at bwynt rheoli'r
cyfansoddwr, sy'n gwirio bod gan yr alwr `CAP_STORAGE_ADMIN` cyn
gweithredu. Adroddir gwrthodiad ar yr allbwn gwall gyda chod gadael nad
yw'n sero; ni ddyfeisir dim ac ni thybir unrhyw awdurdod.

Rhoddir yn union un modd fesul galwad.

Nid oes gan TAIRiX `/dev`, felly ysgrifennir yn wahanol yma y ddau enw y
mae Linux mdadm yn eu hysgrifennu fel ffeiliau dyfais — gwahaniaeth
bwriadol wedi'i ddogfennu:

- Enwir dyfais yn ôl dynodwr ei nod yn y goeden galedwedd, wedi'i
  ysgrifennu `node:<id>`, yr un enw y mae'r adroddiadau'n ei ddangos.
  Gwrthodir unrhyw sillafiad arall yn hytrach na dyfalu.
- Enwir arae yn ôl ei hunaniaeth 128-did mewn hecsadegol. Derbynnir yr
  hunaniaeth lawn o 32 digid, yn ogystal ag unrhyw ragddodiad sy'n enwi'n
  union un arae; gwrthodir rhagddodiad sy'n cyfateb i fwy nag un arae yn
  hytrach na dyfalu pa un a olygwyd.

Mae TAIRiX yn cyfansoddi lefelau RAID 0, 1, 5, 6, 10 a paredd triphlyg.
Nid oes ganddo RAID4, felly gwrthodir `--level=4` gyda'r rheswm hwnnw.

Ysgrifennir cyd-destun cynghori cryno — arae sydd wedi dirywio, neu
ddyfeisiau gwag na ddangosir yn y golwg araeau — ar y ffrwd wybodaeth
safonol (fd 3). Mae'n ddewisol ac nid yw byth yn newid yr allbwn
sylfaenol.

## OPTIONS

- `-C, --create` — creu arae dros y dyfeisiau a enwir ac argraffu'r
  hunaniaeth y mae'r cyfansoddwr yn ei bathu iddi.
- `-D, --detail` — adrodd hunaniaeth, lefel, iechyd, cyfrifon dyfeisiau,
  geometreg ac unrhyw safle ailadeiladu neu wirio pob arae. Heb weithredr
  arae, adrodd pob arae.
- `-E, --examine` — rhestru pob dyfais y mae'r cyfansoddwr yn ei dal:
  aelodau araeau gyda'u slot a'u cyflwr, a'r dyfeisiau gwag heb eu
  cysylltu y gellir creu arae newydd drostynt.
- `-a, --add` — derbyn dyfais wag i slot absennol arae a'i hailadeiladu.
- `-r, --remove` — tynnu dyfais aelod o arae.
- `-S, --stop` — stopio arae fyw a rhyddhau ei haelodau.
- `-l, --level=<level>` — y lefel i'w chreu: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, neu
  `tp`/`raid-tp` ar gyfer paredd triphlyg.
- `-n, --raid-devices=<count>` — nifer y slotiau aelod i'w creu; rhaid
  iddo fod yn gyfartal â nifer y gweithredyddion dyfais.
- `-c, --chunk=<blocks>` — yr uned stribed mewn blociau rhesymegol; dilys
  ar gyfer lefel stribedog yn unig.
- `-h, -?, --help` — dangos cymorth y gorchymyn hwn ei hun.
- `-V, --version` — argraffu'r fersiwn a gadael.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — creu arae RAID5 dros dair dyfais.
- `mdadm --detail` — adrodd pob arae.
- `mdadm --examine` — rhestru pob dyfais, aelodau a rhai gwag fel ei gilydd.
- `mdadm --add 3f2a node:14` — ychwanegu dyfais at yr arae y mae ei hunaniaeth yn dechrau â `3f2a`.
- `mdadm --stop 3f2a` — stopio'r arae honno.

## EXIT STATUS

- `0` — llwyddodd y cais (neu ysgrifennwyd y cymorth).
- `1` — gwrthodwyd gallu, ni ddatryswyd enw, gwrthododd y cyfansoddwr y
  cais, neu ni ellid ysgrifennu'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth hwn (tag BCP-47 fel
  `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
