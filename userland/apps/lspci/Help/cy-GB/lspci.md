## NAME

lspci — rhestru'r dyfeisiau PCI/PCIe a ddarganfuwyd

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Yn rhestru, un llinell i bob swyddogaeth PCI/PCIe a ddarganfuwyd,
dynodwr nod coeden galedwedd y swyddogaeth, ei dosbarth, ac enwau'r
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

Nid yw RustOS yn cofnodi cyfeiriad PCI `bus:device.function`: cyfeiriad
sefydlog swyddogaeth yw dynodwr ei nod yn y goeden galedwedd, a
ddangosir fel `#<node>`, ac mae `-s` yn dewis y dynodwr hwnnw
(gwyriad bwriadol, wedi'i ddogfennu, oddi wrth `lspci` Linux). Nid
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
- `-t` — dangos y swyddogaethau fel coeden o dan eu bysiau rhiant.
- `-d [<vendor>]:[<device>]` — rhestru dim ond y swyddogaethau sy'n
  cyfateb i'r dynodwyr a roddwyd (hecsadegol); mae hanner a
  hepgorwyd yn cyfateb i unrhyw beth.
- `-s <node>` — rhestru dim ond y swyddogaeth â'r dynodwr nod a
  roddwyd (degol).
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `lspci` — pob swyddogaeth PCI a ddarganfuwyd, gydag enwau.
- `lspci -nn` — yr un peth, gyda'r dynodwyr rhifol wrth eu hymyl.
- `lspci -v -s 7` — llinell nod 7 ynghyd â'i adnoddau datganedig.
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
