## NAME

printf — formattare e stampare dati

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Stampa gli `argument` sotto il controllo di `format`, come la funzione C
`printf`. Il formato contiene tre tipi di elementi: caratteri ordinari,
copiati sullo standard output; sequenze di escape con barra rovesciata;
e direttive di conversione `%`, ognuna delle quali converte l'argomento
successivo.

Le sequenze di escape sono `\a` (avviso), `\b` (backspace), `\c`
(terminare subito tutto l'output), `\e` (escape), `\f` (avanzamento
pagina), `\n` (a capo), `\r` (ritorno carrello), `\t` (tabulazione),
`\v` (tabulazione verticale), `\\`, `\"`, `\NNN` (da una a tre cifre
ottali), `\xHH` (una o due cifre esadecimali) e `\uHHHH` / `\UHHHHHHHH`
(punti di codice Unicode, quattro o otto cifre esadecimali).

Le conversioni sono `%d`/`%i` (decimale con segno), `%u` (decimale senza
segno), `%o`/`%x`/`%X` (ottale ed esadecimale), `%e`/`%E`/`%f`/`%F`/
`%g`/`%G`/`%a`/`%A` (virgola mobile), `%c` (il primo carattere
dell'argomento), `%s` (stringa), `%b` (stringa con i propri escape
interpretati, l'ottale si scrive `\0NNN`), `%q` (stringa quotata per il
riuso come input di shell) e `%%` (un `%` letterale). Una direttiva
accetta i flag C `-`, `+`, spazio, `#`, `0` e `'`, una larghezza di
campo e una precisione; larghezza e precisione possono essere `*`,
leggendo il valore dall'argomento successivo. `%b` e `%q` non accettano
flag, larghezza né precisione.

Il formato viene riutilizzato finché ogni argomento non è consumato; una
conversione senza argomenti rimanenti stampa zero o la stringa vuota. Un
argomento numerico è letto come un numero C (esadecimale `0x`, ottale
con `0` iniziale, virgola mobile, `inf`, `nan`); un `'` o `"` iniziale
converte il punto di codice del carattere successivo. Un argomento che
non è un numero, lo è solo in parte o è fuori intervallo viene
diagnosticato sullo standard error e convertito fin dove arriva —
l'esecuzione continua e termina con stato `1`. Una conversione
sconosciuta, un flag su una conversione che non lo accetta o un escape
malformato termina l'esecuzione con una diagnosi.

Due divergenze deliberate dal `printf` GNU: la virgola mobile è
calcolata in doppia precisione IEEE 754 (GNU usa il `long double`),
quindi un valore oltre l'intervallo del double stampa `inf`; e un
*primo* argomento `-h` o `-?` mostra questo aiuto breve — un tale
formato si scrive `printf -- -h...`.

## OPTIONS

- `-h, -?` — mostrare l'aiuto breve di questo comando (solo come primo
  argomento).
- `--` — terminare l'analisi delle opzioni; l'argomento successivo è il
  formato.

## EXAMPLES

- `printf '%s\n' hello` — stampare `hello` e un a capo.
- `printf '%d\n' 0x10` — stampare `16`.
- `printf '%5.2f|\n' 3.14159` — stampare ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — stampare
  `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — stampare due righe da un solo argomento.
- `printf '%s-' a b c` — riutilizzare il formato: `a-b-c-`.

## EXIT STATUS

- `0` — tutto (o l'aiuto breve richiesto) è stato scritto.
- `1` — è stato diagnosticato un problema di conversione, il formato
  mancava o era non valido, un escape era malformato, oppure l'output
  ha smesso di accettare byte.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per l'aiuto breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `seq`
- `man`
