## NAME

man — mostrare il documento di aiuto di un comando

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Mostra il documento di aiuto fornito dal pacchetto applicativo di un
comando, nella tua lingua quando esiste una traduzione.

Ogni programma TAIRiX è un pacchetto applicativo con un albero `Help/`: un
documento strutturato per comando o argomento, per lingua. `man` risolve
`<command>` esattamente come la shell — prima il negozio di applicazioni
di sistema, poi le directory di `PATH` — così la pagina mostrata descrive
sempre il programma che la shell eseguirebbe per la stessa parola. Un
suffisso `.app` nomina direttamente il pacchetto. Quando né il negozio né
`PATH` contengono la parola, `man` percorre i negozi di applicazioni in
modo ricorsivo — prima `/Apps`, poi la cartella `Apps` della propria home
— così un pacchetto riposto in cartelle annidate viene comunque trovato;
la ricerca non guarda mai dentro un altro pacchetto, e vince la
corrispondenza meno profonda.

Il documento è scelto in base alla localizzazione della variabile
d'ambiente `LANG`, con ripiego sulla stessa lingua di un'altra regione e
infine sul documento canonico in inglese. Quando la pagina non è mostrata
nella lingua richiesta, `man` annota la sostituzione sul flusso
consultivo (fd 3); la pagina stessa non mescola mai le lingue.

Su una console interattiva la pagina è mostrata una schermata alla volta:
lo spazio volta pagina, invio avanza di una riga e `q` interrompe. Quando
l'output è rediretto o la dimensione della console è ignota, l'intera
pagina scorre senza pause.

## OPTIONS

- `-h, -?` — mostrare l'aiuto breve di questo comando.

## EXAMPLES

- `man ps` — mostrare la pagina di `ps`.
- `man top keys` — mostrare l'argomento `keys` del pacchetto `top`.
- `man files.app` — nominare direttamente il pacchetto.

## EXIT STATUS

- `0` — la pagina è stata mostrata.
- `1` — il comando o il suo documento di aiuto non è stato trovato,
  oppure la pagina non è stata consegnata.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita (un'etichetta BCP-47 come `it-IT`).
- `PATH` — le directory aggiuntive in cui cercare i pacchetti
  `<command>.app`, dopo il negozio di applicazioni di sistema.
- `HOME` — nomina la propria cartella `Apps` per la ricerca ricorsiva dei
  pacchetti.

## SEE ALSO

- `elsh`
