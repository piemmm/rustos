## NAME

edit — golygydd testun sgrin-lawn

## SYNOPSIS

`edit [file] [-h | -?]`

## DESCRIPTION

Golygydd testun sgrin-lawn yn ysbryd golygydd clasurol QuickBasic /
MS-DOS: bar dewislenni ar draws y brig, y testun oddi tano, a llinell
statws yn dangos enw'r ffeil, safle'r cyrchwr ac awgrymiadau'r
bysellau. Mae'n golygu un ffeil ar y tro.

O'i gychwyn ag operand `file`, mae'r golygydd yn llwytho'r ffeil
honno; mae ffeil nad yw'n bodoli eto'n agor fel byffer gwag ac fe'i
crëir ar y cadw cyntaf. O'i gychwyn heb operand, mae'n agor byffer
dienw ac yn gofyn am enw pan gedwir ef gyntaf.

Mae'r ddewislen (a agorir gydag `F10` neu gydag `Alt` a llythyren
amlygedig teitl — `Alt-F` ar gyfer `File`, `Alt-S` ar gyfer `Search` —
a lywir â'r saethau, `Enter` yn dewis, `Esc` neu `F10` yn cau) yn
cario:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Pan fyddai gweithred yn taflu newidiadau heb eu cadw (`New`,
`Open...`, `Exit`), mae'r golygydd yn gofyn yn gyntaf: mae `y` yn
cadw ac yn parhau, `n` yn taflu, `c` (neu `Esc`) yn diddymu.

Bysellau o fewn y sesiwn:

- Mae teipio'n mewnosod wrth y cyrchwr; mae `Insert` yn toglo
  trosysgrifo (`OVR` ar y llinell statws).
- Mae `Enter` yn hollti'r llinell; mae `Backspace` a `Delete` yn dileu
  nodau ac yn uno llinellau wrth derfynau llinell.
- Mae'r saethau, `Home`, `End`, `PageUp`, `PageDown` yn symud y
  cyrchwr; mae'r olwg yn sgrolio, yn llorweddol hefyd, i'w ddilyn.
- Mae `Tab` yn mewnosod bylchau hyd at yr arhosfan wyth colofn nesaf.
- Mae `F1` yn dangos crynodeb y bysellau, `F2` yn cadw, `F3` yn
  ailadrodd y chwiliad diwethaf, `F10` (neu `Alt-F` / `Alt-S`) yn
  agor y ddewislen.

Mae `Find...` yn chwilio ymlaen o'r cyrchwr, yn llythrennol a chan
wahaniaethu prif lythrennau, gan lapio o amgylch ar ddiwedd y byffer;
mae chwiliad heb gydweddiad yn adrodd `Match not found` ac yn gadael
y cyrchwr lle'r oedd.

Dim ond ffeiliau testun y mae'r golygydd yn eu golygu, ac mae'n dweud
yn union beth mae'n ei newid:

- Rhaid i'r ffeil fod yn destun UTF-8 heb fod yn fwy na 16 MiB;
  gwrthodir unrhyw beth arall (ffeil ddeuaidd, dychweliad cerbyd
  unigol, ffeil rhy fawr) gyda'r rheswm wedi'i ddatgan — ni agorir
  byth fel sbwriel.
- Ehangir nodau tab yn fylchau wrth arosfannau wyth colofn wrth
  lwytho, a daw terfyniadau llinell CRLF yn LF; cyhoeddir pob
  trosiad ar y llinell statws, ni chymhwysir byth yn dawel.
- Cedwir presenoldeb neu absenoldeb llinell newydd derfynol y ffeil.

Adroddir am lwytho neu gadw a fethodd o fewn y sesiwn ar y llinell
statws a chedwir y byffer; nid yw'r sesiwn byth yn marw dros ffeil a
wrthodwyd. Caiff pob llwybr ei ddatrys a'i wirio o ran caniatâd gan y
cnewyllyn o dan hunaniaeth y galwr ei hun — nid yw'r golygydd yn dal
unrhyw awdurdod arbennig.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

- `0` — daeth y sesiwn i ben trwy `File > Exit`, neu dangoswyd y
  cymorth byr.
- `1` — ni ellid llwytho'r ffeil a enwyd (nid testun, rhy fawr, neu
  wedi'i gwrthod), neu methodd y derfynell; argraffir y rheswm ar y
  gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).
- `TERM` — y derfynell y mae'r sesiwn yn tynnu ar ei chyfer; mae
  gwerth anhysbys neu goll yn dirywio i sylfaen ddiogel.

## SEE ALSO

- `cat`
- `man`
