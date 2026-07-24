## NAME

configure — darllen a gosod cyfluniad y system adeg cychwyn

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Yn rhestru, dangos a gosod gosodiadau'r storfa gyfluniad yn
`/System/Settings/Configuration/system.conf`. Heb operand rhestrir pob
gosodiad gyda'i werth cyfredol; gydag allwedd yn unig dangosir gwerth y
gosodiad hwnnw; gydag allwedd a gwerth newidir y gosodiad.

Mae'r storfa'n byw ar y gyfrol wraidd wedi'i hamgryptio ac fe'i darllenir
gan ei defnyddwyr ar ôl datgloi'r system ffeiliau wraidd; daw newid i
rym y tro nesaf y bydd ei ddefnyddiwr yn cychwyn (`os.loginType`:
mewngofnodi'r cychwyniad nesaf; y switshis `cache.*`: datgloi'r
cychwyniad nesaf).

Mae'r set allweddi ar gau: gwrthodir allwedd anhysbys, neu werth y tu
allan i set allwedd, gan nodi'r dewisiadau dilys a heb newid dim. Mae
newid gosodiad yn ailysgrifennu'r storfa yn ei ffurf ganonaidd ac yn
gofyn am fynediad ysgrifennu i `/System/Settings` — gall cyfrif
cyffredin ddarllen y gosodiadau ond nid eu newid.

- `os.loginType` — `text` neu `graphical`: pa fath o sesiwn y mae'r
  gwasanaeth mewngofnodi yn ei chychwyn i ddefnyddiwr wedi'i ddilysu.
  Mae `text` (y rhagosodiad) yn cychwyn cragen y cyfrif — gellir dal i
  gychwyn y bwrdd gwaith ar alw gyda'r gorchymyn `desktop`; mae
  `graphical` yn cychwyn y sesiwn bwrdd gwaith yn uniongyrchol ar ôl
  dilysu pan fo bwrdd gwaith wedi'i osod, gan ddisgyn yn ôl i destun
  pan nad oes un.
- `cache.all` — `on` neu `off`: y prif switsh storio dros dro. Mae
  `on` (y rhagosodiad) yn gadael i bob dosbarth storfa isod ddilyn ei
  osodiad ei hun; mae `off` yn nenfwd sy'n analluogi pob storfa gof
  waeth beth fo'r gosodiadau fesul dosbarth.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` neu `off`: y switshis fesul dosbarth ar
  gyfer y pedair storfa gof adenilladwy (storfeydd y system ffeiliau,
  y blociau disg cyfan, y clystyrau wedi'u datgywasgu, a chychwyn
  cymwysiadau). Mae `auto` (y rhagosodiad) yn gadael i reolwr pwysau'r
  cof lywodraethu'r dosbarth; mae `off` yn ei analluogi'n llwyr. Nid
  oes `on` fesul dosbarth: ni ellir gorfodi dosbarth i anwybyddu pwysau
  cof. Mae dosbarth yn effeithiol `off` pryd bynnag y bo `cache.all`
  yn `off`.

Mae pob storfa'n gyflymydd adenilladwy, byth yn ffynhonnell y gwir,
felly nid yw diffodd unrhyw un neu bob un ohonynt ond yn gwneud y gwaith
dan sylw yn arafach — nid yw byth yn newid canlyniad.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `configure` — rhestru pob gosodiad.
- `configure os.loginType` — dangos y math sesiwn rhagosodedig.
- `configure os.loginType graphical` — cychwyn i'r mewngofnodi
  graffigol.
- `configure cache.all off` — analluogi pob storfa gof ar draws y
  system gyfan.
- `configure cache.filesystem off` — analluogi storfa'r system
  ffeiliau yn unig.

## EXIT STATUS

- `0` — cwblhawyd y rhestr, y gwerth, y cymorth byr neu'r newid.
- `1` — ni ellid darllen nac ysgrifennu'r storfa (er enghraifft ni chaiff
  y galwr newid gosodiadau'r system), neu ni ellid cyflwyno'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn, mae'r allwedd yn anhysbys neu
  mae'r gwerth y tu allan i set yr allwedd.

## ENVIRONMENT

- `LANG` — hoff iaith y cymorth byr (tag BCP-47 fel `fr-FR`).

## SEE ALSO

- `man`
