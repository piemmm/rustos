## NAME

fstree — il gestore di file ad albero a schermo intero

## SYNOPSIS

`fstree [directory]`

## DESCRIPTION

Esplora il file system in una sessione a schermo intero guidata dalla
tastiera: un pannello con l'albero delle directory a sinistra e un
pannello dei file a destra che elenca le voci della directory selezionata
con dimensioni e data di modifica. La sessione parte da `directory`
(la vista radice `/` se omessa).

L'albero è letto pigramente: il contenuto di una directory viene
recuperato solo quando è mostrata o espansa per la prima volta, così
esplorare un volume enorme costa solo le directory realmente aperte. Una
directory che il chiamante non può elencare è rifiutata sul posto:
l'errore compare sulla riga dei messaggi e la vista precedente resta
com'era; nulla viene inventato.

Tasti:

- `Su`/`Giù` o `k`/`j` — muovere il cursore del pannello attivo. Muovendo
  il cursore dell'albero, la directory appena selezionata viene elencata
  nel pannello dei file.
- `Sinistra`/`Destra` o `h`/`l` — comprimere/espandere la riga dell'albero
  sotto il cursore.
- `Invio` — nell'albero alterna l'espansione; nel pannello dei file scende
  nella directory selezionata (entrambi i pannelli seguono).
- `Tab` — cambiare il pannello attivo.
- `s` — aprire il menu di ordinamento: `n` nome, `e` estensione,
  `s` dimensione, `m` data di modifica, `r` inverte il verso, `Esc`
  annulla. Le directory sono sempre raggruppate prima dei file.
- `.` — mostrare/nascondere le voci nascoste (nomi con punto) in entrambi
  i pannelli.
- `?` — mostrare questo aiuto sopra i pannelli; qualsiasi tasto lo chiude.
- `q` — uscire ripristinando il terminale.

La riga di stato mostra il percorso elencato, il numero di voci visibili,
l'ordinamento, i byte liberi/totali del volume sottostante (quando il
servizio di informazioni di sistema può riferirli) e se le voci nascoste
sono visibili. Un file il cui formato di archiviazione non conserva la
data di modifica mostra `-` nella colonna della data.

Le operazioni sui file (copia, spostamento, rinomina, eliminazione), la
marcatura, la ricerca e i visualizzatori testo/esadecimale/disassemblato
arrivano nelle fasi successive del piano dello strumento.

## OPTIONS

- `directory` — la directory in cui inizia la sessione; il valore
  predefinito è la vista radice `/`.
- `-h`, `-?` — stampare la forma breve di questo documento e uscire.

## EXIT STATUS

- `0` — la sessione è terminata con la `q` dell'utente.
- `1` — la directory iniziale non è stata elencabile, o il percorso del
  terminale è fallito.
- `2` — gli argomenti non sono stati compresi.

## SEE ALSO

ls, du, df
