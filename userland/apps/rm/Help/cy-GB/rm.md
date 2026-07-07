## NAME

rm — dileu ffeiliau a chyfeiriaduron

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Mae'n dileu pob operand ffeil, yn eu trefn. Datgysylltir operand nad
yw'n gyfeiriadur; dim ond gydag `-r` y dilëir operand cyfeiriadur
(sy'n dileu ei gynnwys ddyfnder-yn-gyntaf ac yna'r cyfeiriadur ei hun)
neu, pan fo'n wag, gyda `-d`.

Gydag `-f`, hepgorir operand nad yw'n bodoli yn dawel ac ni ofynnir
byth gwestiwn. Mae `-i` yn gofyn ar ffrwd y gwall safonol cyn pob
dileu a chyn disgyn i gyfeiriadur; mae `-I` yn gofyn unwaith ymlaen
llaw cyn dileu mwy na thri operand neu cyn dileu ailadroddus. Mae
cwestiwn a wrthodwyd yn hepgor y gwrthrych (neu'r rhediad cyfan, yn
achos `-I`) heb wall; ni thrinnir ateb annarllenadwy byth fel
cydsyniad. Y diweddaraf o `-f`, `-i` ac `-I` sy'n ennill.

Gwrthodir yr operand `/` o dan `--preserve-root`, y rhagosodiad. Mae'r
methiant cyntaf yn atal y rhediad cyn unrhyw operand diweddarach. Mae
`--` yn gorffen dosrannu opsiynau: mae pob ymresymiad diweddarach yn
llwybr.

## OPTIONS

- `-r, -R, --recursive` — dileu cyfeiriaduron a'u cynnwys.
- `-f, --force` — anwybyddu operandau nad ydynt yn bodoli; peidio byth
  â holi.
- `-d, --dir` — dileu cyfeiriaduron gwag.
- `-i, --interactive` — holi cyn pob dileu; dim ond ateb yn dechrau ag
  `y`/`Y` sy'n cydsynio.
- `-I` — holi unwaith cyn dileu mwy na thri operand, neu cyn dileu
  ailadroddus.
- `-v, --verbose` — adrodd am bob dileu fel `removed 'file'`.
- `--preserve-root` — gwrthod dileu `/` (y rhagosodiad).
- `--no-preserve-root` — caniatáu dileu `/`.
- `-h, -?, --help` — dangos cymorth byr y gorchymyn hwn ei hun.

## EXAMPLES

- `rm notes.txt` — dileu un ffeil.
- `rm -r Scratch` — dileu coeden `Scratch` a phopeth ynddi.
- `rm -I a b c d` — gofyn unwaith, yna dileu'r pedair ffeil ar `y`.

## EXIT STATUS

- `0` — llwyddodd pob dileu (nid yw cwestiwn a wrthodwyd na hepgoriad
  `-f` yn fethiannau).
- `1` — methiant system ffeiliau, anogwr neu allbwn; argraffir y
  rheswm ar y gwall safonol.
- `2` — ni ddeallwyd y llinell orchymyn.

## ENVIRONMENT

- `LANG` — y locale a ffefrir ar gyfer y cymorth byr (tag BCP-47 fel
  `cy-GB`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
