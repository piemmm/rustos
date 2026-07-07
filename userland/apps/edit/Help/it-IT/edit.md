## NAME

edit — editor di testo a schermo intero

## SYNOPSIS

`edit [file] [-h | -?]`

## DESCRIPTION

Un editor di testo a schermo intero nello spirito del classico editor
QuickBasic / MS-DOS: una barra dei menu in alto, il testo sotto e una
riga di stato con il nome del file, la posizione del cursore e i
tasti principali. Modifica un file alla volta.

Avviato con un operando `file`, l'editor carica quel file; un file
non ancora esistente si apre come buffer vuoto e viene creato al
primo salvataggio. Avviato senza operando, apre un buffer senza nome
e chiede un nome al primo salvataggio.

Il menu (si apre con `F10` o con `Alt` più la lettera evidenziata di
un titolo — `Alt-F` per `File`, `Alt-S` per `Search` —, si percorre
con le frecce, `Enter` seleziona, `Esc` o `F10` chiude) offre:

- `File` — `New`, `Open...`, `Save`, `Save As...`, `Exit`.
- `Search` — `Find...`, `Repeat Last Find`.

Quando un'azione scarterebbe modifiche non salvate (`New`, `Open...`,
`Exit`), l'editor chiede prima: `y` salva e prosegue, `n` scarta,
`c` (o `Esc`) annulla.

Tasti nella sessione:

- La digitazione inserisce al cursore; `Insert` alterna la modalità
  di sovrascrittura (`OVR` nella riga di stato).
- `Enter` divide la riga; `Backspace` e `Delete` cancellano
  caratteri e uniscono le righe ai fine riga.
- Le frecce, `Home`, `End`, `PageUp`, `PageDown` muovono il cursore;
  la vista scorre, anche in orizzontale, per seguirlo.
- `Tab` inserisce spazi fino alla prossima tabulazione di otto
  colonne.
- `F1` mostra il riepilogo dei tasti, `F2` salva, `F3` ripete
  l'ultima ricerca, `F10` (o `Alt-F` / `Alt-S`) apre il menu.

`Find...` cerca in avanti dal cursore, letteralmente e distinguendo
maiuscole e minuscole, riprendendo dall'inizio alla fine del buffer;
una ricerca senza esito segnala `Match not found` e lascia il
cursore dov'era.

L'editor modifica solo file di testo, e dichiara esattamente ciò che
cambia:

- Il file deve essere testo UTF-8 di al massimo 16 MiB; tutto il
  resto (un file binario, un ritorno a capo isolato, un file troppo
  grande) viene rifiutato indicandone il motivo — mai aperto come
  spazzatura.
- Le tabulazioni sono espanse in spazi su fermate di otto colonne al
  caricamento, e i fine riga CRLF diventano LF; ogni conversione è
  annunciata nella riga di stato, mai applicata in silenzio.
- La presenza o l'assenza dell'a capo finale del file viene
  preservata.

Un caricamento o un salvataggio rifiutato durante la sessione viene
segnalato nella riga di stato e il buffer resta intatto; la sessione
non muore mai per un file rifiutato. Ogni percorso è risolto e
verificato dal kernel sotto l'identità del chiamante — l'editor non
detiene alcuna autorità speciale.

## OPTIONS

- `-h, -?` — mostrare la breve guida di questo comando e uscire.

## EXIT STATUS

- `0` — la sessione è terminata tramite `File > Exit`, oppure è
  stata mostrata la breve guida.
- `1` — il file indicato non è stato caricato (non è testo, è troppo
  grande o è stato rifiutato), oppure il terminale è venuto meno; il
  motivo è stampato sull'uscita di errore.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la breve guida
  (un'etichetta BCP-47 come `it-IT`).
- `TERM` — il terminale per cui la sessione disegna; un valore
  sconosciuto o assente degrada a una base sicura.

## SEE ALSO

- `cat`
- `man`
