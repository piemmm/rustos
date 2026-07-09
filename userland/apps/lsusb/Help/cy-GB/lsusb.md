## NAME

lsusb — rhestru'r dyfeisiau USB a ddarganfuwyd

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Yn rhestru, un llinell fesul rhyngwyneb USB a ddarganfuwyd, rifau bws
a dyfais y rhyngwyneb, ei ddynodwr `vendor:product`, ac enwau'r
gwneuthurwr a'r cynnyrch. Y rhestr yw'r goeden galedwedd — unig restr
ddyfeisiau'r system — a ddarllenir trwy API gwybodaeth y system, sy'n
gofyn am y gallu `CAP_SYSINFO_HW`; adroddir gwrthodiad ar y gwall
safonol ac ni restrir dim yn ei le.

Daw'r enwau o'r ciplun dilysedig o'r gronfa ddata gyhoeddus o
ddynodwyr USB y mae'r gorchymyn hwn yn ei chludo yn ei becyn ei hun.
Dangosir hunaniaeth nad yw'r gronfa'n ei henwi yn ei ffurf rifol
`ID vvvv:pppp` yn unig, byth wedi'i dyfeisio, a nodir nifer y
dyfeisiau hynny ar y ffrwd gwybodaeth safonol (fd 3). Os yw'r tabl a
gludir ar goll neu'n methu'r dilysu, mae'r rhestr yn dirywio i
ddynodwyr noeth gyda'r rheswm ar y gwall safonol — rhestrir y rhestr
ei hun o hyd.

Nid oes gan RustOS gofrestr rhifau bws/dyfais Linux: rhif bws dyfais
yw rhif nod sefydlog ei rheolydd yn y goeden galedwedd, a'i rhif
dyfais yw ei rhif nod ei hun, ac mae `-s` yn dewis y rhifau nod hynny
(gwyriad bwriadol, wedi'i ddogfennu, oddi wrth `lsusb` Linux). Mae'r
rhestr yn cofnodi un nod fesul *rhyngwyneb*: mae dyfais aml-ryngwyneb
yn ymddangos unwaith fesul rhyngwyneb.

## OPTIONS

- `-v` — ar ôl pob dyfais, rhestru dosbarth, is-ddosbarth a phrotocol
  ei rhyngwyneb (`bInterfaceClass`, `bInterfaceSubClass`,
  `bInterfaceProtocol`) gyda'r enwau o dablau dosbarth USB.
- `-t` — dangos y dyfeisiau fel coeden o dan eu rheolyddion a'u
  bysiau.
- `-d [<vendor>]:[<product>]` — rhestru dim ond y dyfeisiau sy'n cyfateb
  i'r dynodwyr gwneuthurwr/cynnyrch a roddir (hecs); mae hanner a
  hepgorir yn cyfateb i unrhyw un.
- `-s [[<bus>]:][<devnum>]` — rhestru dim ond y dyfeisiau sy'n cyfateb
  i rifau nod y rheolydd (bws) a/neu'r ddyfais (degol); rhif dyfais yn
  unig yw gwerth heb golon.
- `-?, --help` — dangos cymorth byr y gorchymyn hwn.

## EXAMPLES

- `lsusb` — pob dyfais USB a ddarganfuwyd, gydag enwau.
- `lsusb -v` — yr un peth, gyda hunaniaeth dosbarth pob rhyngwyneb.
- `lsusb -s 2:` — pob dyfais o dan nod rheolydd 2.
- `lsusb -d 046d:` — pob dyfais gan y gwneuthurwr `046d` (Logitech).
- `lsusb -t` — y dyfeisiau o dan eu topoleg bws.

## EXIT STATUS

- `0` — ysgrifennwyd y rhestr (neu'r cymorth byr).
- `1` — gwrthodwyd ymholiad y goeden galedwedd neu fe fethodd, neu ni
  ellid ysgrifennu'r allbwn.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
