## NAME

files — porwr graffigol y system ffeiliau

## SYNOPSIS

`files [directory] [-h | -?]`

## DESCRIPTION

Yn agor ffenestr bwrdd gwaith sy'n rhestru'r system ffeiliau, gan
ddechrau o'r `directory` a enwir ar y llinell orchymyn, neu o
gyfeiriadur cartref y defnyddiwr a'i lansiodd pan na enwir yr un. Mae'r
rhes uchaf yn dangos llwybr y cyfeiriadur presennol; mae'r rhesi oddi
tani yn rhestru cofnodion y cyfeiriadur, gyda'r cofnod a ddewiswyd
wedi'i amlygu â lliw acen y thema weithredol. Mae pob darlleniad
cyfeiriadur yn rhestriad cyffredin wedi'i wirio gan ganiatâd o dan
hunaniaeth y defnyddiwr a'i lansiodd: gwrthodir cyfeiriadur
annarllenadwy, ni ddyfelir byth.

Lansir y porwr o fotwm parhaol `Files` y bar tasgau
neu wrth ei enw o gragen. Mae angen sesiwn graffigol weithredol arno:
hebddi, mae'r sianel ffenestr yn anghyraeddadwy ac mae'r porwr yn nodi'r
gwrthodiad ar y ffrwd gwall safonol ac yn gorffen.

Rheolir y ffenestr â'r bysellfwrdd: mae `I lawr` ac `I fyny` yn symud y
dewisiad, mae `Enter` yn agor y cyfeiriadur a ddewiswyd, ac mae
`Backspace` yn codi i'r cyfeiriadur rhiant. Mae cau'r ffenestr o'r bwrdd
gwaith yn gorffen y porwr.

Trinnir yr operand `directory` fel mewnbwn nad ymddiredir ynddo: rhaid
iddo fod yn llwybr absoliwt o fewn terfyn hyd llwybr y system, a rhaid
i bob un o'i gydrannau fod yn enw cyfeiriadur go iawn — nid yw `.` a
`..` yn rhai felly, fel na all sillafiad byth olygu rhywle heblaw'r hyn
y mae i'w ddarllen. Gwrthodir cyfeiriadur sy'n torri unrhyw un o'r
rheolau hynny, neu na all y defnyddiwr a'i lansiodd ei restru, gyda'r
rheswm ar y ffrwd gwall safonol, ac yna mae'r ffenestr yn agor yn y
cyfeiriadur cartref yn lle hynny, fel nad yw ymresymiad gwael byth yn
gadael y defnyddiwr heb ffenestr. Gwrthodir ail operand yn llwyr yn
hytrach na'i anwybyddu.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

Sero ar ôl cau glân, neu ar ôl dangos y cymorth byr; `2` pan na
ddeallwyd y llinell orchymyn; fel arall heb fod yn sero pan wrthodwyd y
sianel ffenestr, y rhanbarth fframiau a rennir, neu restriad cychwynnol
y cyfeiriadur (nodir y rheswm ar y ffrwd gwall safonol).
