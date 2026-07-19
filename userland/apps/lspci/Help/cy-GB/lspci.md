## NAME

lspci — rhestru'r dyfeisiau PCI/PCIe a ddarganfuwyd

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Yn rhestru, un llinell i bob swyddogaeth PCI/PCIe a ddarganfuwyd, rhif
rhestr bach, dosbarth y swyddogaeth, ac enwau'r
gwneuthurwr a'r ddyfais. Y rhestr yw'r goeden galedwedd — unig
restr ddyfeisiau'r system — a ddarllenir drwy API gwybodaeth y system,
sy'n gofyn am y gallu `CAP_SYSINFO_HW`; adroddir gwrthodiad ar y gwall
safonol ac ni restrir dim yn ei le.

Daw'r enwau o giplun dilysedig o'r gronfa gyhoeddus o ddynodwyr PCI y
mae'r gorchymyn hwn yn ei chludo yn ei becyn ei hun. Dangosir
hunaniaeth nad yw'r gronfa'n ei henwi ar ffurf rifol (`Vendor 8086`,
`Device 2922`, `Class 0106`), byth wedi'i dyfeisio, a nodir nifer y
dyfeisiau o'r fath ar y ffrwd wybodaeth safonol (fd 3). Os yw'r tabl
sydd wedi'i gynnwys ar goll neu'n methu'r dilysu, mae'r rhestr yn
dirywio i ddynodwyr rhifol gyda'r rheswm ar y gwall safonol — rhestrir
y rhestr ei hun o hyd.

Nid yw TAIRiX yn cofnodi cyfeiriad PCI `bus:device.function`. Yn lle
hynny rhoddir i bob dyfais a restrir rif bach, sefydlog a neilltuir yn
nhrefn y bws, a ddangosir fel `#<n>`, ac mae `-s` yn dewis y rhif hwnnw
(gwyriad bwriadol, wedi'i ddogfennu, oddi wrth `lspci` Linux). Nid
dynodwr nod mewnol y goeden galedwedd yw'r rhif hwn; daw'r dynodwr nod
o le neilltuedig a gall fod yn werth mawr, diystyr. Nid
yw'r olwg `-k` (gyrrwr cnewyllyn) ar gael eto: nid yw'r system yn
cyhoeddi cofnodion rhwymo gyrwyr, ac mae `lspci` ond yn adrodd yr hyn
y mae'r system yn ei gofnodi mewn gwirionedd.

## OPTIONS

- `-n` — dynodwyr rhifol yn unig: cod y dosbarth a `vendor:device`
  mewn hecsadegol.
- `-nn` — yr enwau ac yna'r dynodwyr rhifol mewn cromfachau sgwâr.
- `-v` — ar ôl pob swyddogaeth, rhestru'r adnoddau y mae ei nod yn eu
  datgan (ffenestri MMIO, llinellau IRQ, pyrth M/A, cyfyngiadau DMA)
  — y ceisiadau grant a gofnodwyd, nid cyflwr byw.
- `-t` — dangos y swyddogaethau fel coeden o dan eu bysiau rhiant;
  mae pob llinell bws ganol yn enwi ei dosbarth a hunaniaeth ei allwedd
  gyfatebol, a chyda `-v` (`-tv`) yn dangos yr adnoddau y mae'n eu
  datgan hefyd.
- `-d [<vendor>]:[<device>]` — rhestru dim ond y swyddogaethau sy'n
  cyfateb i'r dynodwyr a roddwyd (hecsadegol); mae hanner a
  hepgorwyd yn cyfateb i unrhyw beth.
- `-s <node>` — rhestru dim ond y swyddogaeth â'r rhif rhestr a
  roddwyd (y `#<n>` degol a ddangosir yn y rhestr).
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `lspci` — pob swyddogaeth PCI a ddarganfuwyd, gydag enwau.
- `lspci -nn` — yr un peth, gyda'r dynodwyr rhifol wrth eu hymyl.
- `lspci -v -s 7` — llinell dyfais `#7` ynghyd â'i adnoddau datganedig.
- `lspci -d 1af4:` — pob swyddogaeth gan y gwneuthurwr `1af4`
  (virtio).
- `lspci -t` — y swyddogaethau o dan eu topoleg bysiau.

## EXIT STATUS

- `0` — ysgrifennwyd y rhestr (neu'r cymorth byr).
- `1` — gwrthodwyd ymholiad y goeden galedwedd neu methodd, neu ni
  ellid ysgrifennu'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `sysinfo`
- `man`
