## NAME

link — dare a un file un secondo nome

## SYNOPSIS

`link [--] esistente nuovo`

## DESCRIPTION

Crea un collegamento fisico: `nuovo` diventa un secondo nome del nodo che
`esistente` già nomina. I due nomi raggiungono allora lo stesso file — una
scrittura tramite uno è visibile tramite l'altro, perché c'è un file e non
una copia — e lo spazio del file sopravvive fino alla rimozione dell'ultimo
dei suoi nomi.

Non ci sono deliberatamente opzioni. `ln` è lo strumento con `-f`, `-i`,
`-v`, `-s`, `-L`/`-P` e le forme di destinazione `-t`/`-T`; tenerli
separati significa che uno script che deve creare un solo collegamento
fisico e nient'altro ha uno strumento che non può sostituire un nome,
seguire un collegamento o crearne uno simbolico al suo posto.

Nessuno dei due nomi è seguito. `esistente` è il nodo **così come è
scritto**, così un collegamento simbolico piazzato lì non può dirottare il
nuovo nome sul suo bersaglio (`ln -L` è lo strumento per la postura che
segue). `nuovo` è un nome che si crea: uno occupato è rifiutato, mai
sostituito.

Ogni rifiuto dice qualcosa di diverso:

- il nuovo nome esiste già: una creazione non sostituisce mai un nome;
- `esistente` è una **directory**: una directory ha esattamente un nome
  in ogni caso, quindi nessun principale può dargliene un secondo;
- i due nomi sono su **volumi diversi**: il secondo nome di un nodo deve
  stare sul volume che lo memorizza;
- il conteggio dei nomi per nodo del formato andrebbe in overflow;
- il filesystem memorizza **un nome per nodo**: una proprietà permanente
  di quel formato, non un guasto passeggero. Là si usa `ln -s` per un
  collegamento simbolico.

Servono esattamente due operandi; qualunque altra cosa è un errore d'uso e
nessun collegamento è creato. `--` termina l'analisi delle opzioni.

## OPTIONS

- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `link rapporto.txt rapporto-copia.txt` — un secondo nome per un file.
- `link -- -nome-strano secondo` — collegare un nome che inizia con un
  trattino.

## EXIT STATUS

- `0` — il collegamento è stato creato (o è stata scritta la guida breve).
- `1` — il filesystem ha rifiutato il collegamento, o l'output è fallito;
  la ragione è stampata sull'errore standard.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la lingua preferita per la guida breve (un'etichetta BCP-47
  come `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
