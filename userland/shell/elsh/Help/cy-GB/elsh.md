## NAME

elsh — cragen orchmynion RustOS

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

Mae'n rhedeg cragen orchmynion ryngweithiol — dolen darllen-gwerthuso-
argraffu dros y ffrydiau safonol a etifeddwyd. Datrysir gair gorchymyn
a deipiwyd yn gyntaf yn erbyn builtins y gragen, yna storfa raglenni'r
system (`/System/Apps`), yna cyfeiriaduron y newidyn `PATH`; chwilir y
storfa cyn `PATH`, felly ni all `PATH` byth gysgodi gorchymyn system.
Mae gair heb ei ddatrys yn gadael â `127`; mae bwndel a ddatryswyd ond
nad yw'n weithredadwy yn gadael â `126`.

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

## OPTIONS

- `-h, -?` — dangos cymorth byr y gorchymyn hwn ei hun a gadael.

## EXIT STATUS

- Cod y builtin `exit`, neu `0` pan ddaw'r ffrwd fewnbwn i ben (neu
  pan ddangoswyd y cymorth byr).
- `2` — ni ddeallwyd yr alwad.

## ENVIRONMENT

- `PATH` — y cyfeiriaduron a chwilir ar ôl storfa raglenni'r system.
- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`), wedi'i hallforio i bob gorchymyn a lansir.

## SEE ALSO

- `man`
