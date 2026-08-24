## NAME

elsh — cragen orchmynion TAIRiX

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Mae'n rhedeg cragen orchmynion ryngweithiol — dolen darllen-gwerthuso-
argraffu dros y ffrydiau safonol a etifeddwyd. Datrysir gair gorchymyn
a deipiwyd yn gyntaf yn erbyn builtins y gragen, yna storfa orchmynion
y system (`/System/Commands`), storfa raglenni'r system
(`/System/Applications`), storfa orchmynion (`<home>/Commands`) a
storfa raglenni (`<home>/Applications`) y defnyddiwr ei hun, yna
cyfeiriaduron y newidyn `PATH`; mae'r pedair storfa hyn yn rhagddodiad
sefydlog na all y defnyddiwr ei aildrefnu na'i ddisodli, felly ni all
`PATH` byth gysgodi gorchymyn system. Mae gair heb ei ddatrys yn gadael
â `127`; mae bwndel a ddatryswyd ond nad yw'n weithredadwy yn gadael â
`126`.

Y builtins:

- `cd <path>`, `pwd` — newid ac argraffu'r cyfeiriadur gwaith.
- `echo ...` — argraffu ei operandau.
- `export NAME=value`, `unset NAME` — golygu'r amgylchedd a
  allforiwyd.
- `jobs`, `fg`, `bg` — rheoli swyddi.
- `ulimit` — darllen a gosod terfynau adnoddau.
- `elevate` — rhedeg un gorchymyn wedi'i ail-ddilysu trwy oruchwyliwr
  mewngofnodi'r consol.
- `help` — rhestru'r builtins.
- `exit [code]` — gorffen y sesiwn.

Nid yw'r gragen yn cymryd operandau: nid yw rhedeg sgriptiau eto'n
rhan o'i gramadeg.

Ar derfynell mae'r gragen yn cynnig golygydd llinell rhyngweithiol: mae
I fyny/I lawr yn pori hanes y gorchmynion, mae `Ctrl-R` yn ei chwilio,
mae `Ctrl-C` yn gollwng y llinell gyfredol, mae `Ctrl-D` ar linell wag
yn gorffen y sesiwn, ac mae Tab yn cwblhau enwau gorchmynion, llwybrau a
chyfeiriadau adnoddau fel `sys:random`. Mae gofod enwau yn cwblhau ei
ddetholyddion cofrestredig fesul segment
(`state:` → `net/` → `wan/` → `link`). Lle mae'r gofrestr yn gwybod y siâp
yn unig — enw rhyngwyneb, llinell ymyriad — mae Tab yn cynnig enwau
gwirioneddol y peiriant, neu ddim o gwbl os nad yw'r sesiwn hon yn cael eu
rhestru. Mae `info:`/`state:`/`stats:` yn ymddangos fel ymresymiad ac ar ôl `<`,
sy'n darllen y gwerth (`cat < info:mem/physical`), ond byth ar ôl `>`: ni
chaiff adnodd o'r fath ei ysgrifennu drwy ailgyfeirio.

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

- Cod y builtin `exit`, neu `0` pan ddaw'r ffrwd fewnbwn i ben (neu
  pan ddangoswyd y cymorth byr).
- `2` — ni ddeallwyd yr alwad.

## ENVIRONMENT

- `PATH` — y cyfeiriaduron a chwilir ar ôl y rhagddodiad sefydlog o
  storfeydd.
- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`), wedi'i hallforio i bob gorchymyn a lansir.

## SEE ALSO

- `man`
