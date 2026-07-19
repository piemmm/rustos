## NAME

fstree — il gestore di file ad albero a schermo intero

## SYNOPSIS

`fstree [directory]`

## DESCRIPTION

Esplora il file system in una sessione a schermo intero guidata dalla
tastiera, sul modello di XTree Gold: in alto un'intestazione con le
statistiche del disco, una finestra con l'albero delle directory sopra
una finestra dei file che elenca le voci della directory evidenziata con
dimensioni e data di modifica. La sessione parte da `directory` (la vista
radice `/` se omessa).

L'albero è letto pigramente: il contenuto di una directory viene
recuperato solo quando è mostrata o espansa per la prima volta, così
esplorare un volume enorme costa solo le directory realmente aperte. Una
directory che il chiamante non può elencare è rifiutata sul posto:
l'errore compare sulla riga dei messaggi e la vista precedente resta
com'era; nulla viene inventato.

Tasti:

- `Su`/`Giù` o `k`/`j` — muovere l'evidenziazione della finestra attiva
  (`PgSu`/`PgGiù` di una schermata, `Inizio`/`Fine` all'inizio o alla
  fine). Muovendo l'evidenziazione dell'albero, la directory appena
  evidenziata viene elencata nella finestra dei file.
- `Destra`/`l`/`+` — nell'albero, espandere il ramo evidenziato (letto
  pigramente); nella finestra dei file, scendere nella directory
  evidenziata.
- `Sinistra`/`h`/`-` — nell'albero, comprimere il ramo evidenziato o —
  quando è già compresso o senza sottodirectory — risalire
  l'evidenziazione alla directory madre; nella finestra dei file, tornare
  all'albero.
- `Invio` — nell'albero, passare alla finestra dei file; nella finestra
  dei file, aprire la voce evidenziata (una directory viene aperta, un
  file viene mostrato).
- `Esc` — annullare un calcolo dell'uso del disco in corso; altrimenti
  tornare dalla finestra dei file all'albero.
- `Tab` — passare tra la finestra dell'albero e la finestra dei file.
- `s` — aprire il menu di ordinamento: `n` nome, `e` estensione,
  `s` dimensione, `m` data di modifica, `r` inverte il verso, `Esc`
  annulla. Le directory sono sempre raggruppate prima dei file.
- `c` — copiare la voce selezionata: una riga chiede la destinazione.
  Una destinazione relativa finisce nella directory elencata; una
  destinazione che è una directory esistente riceve la copia al suo
  interno, con il nome dell'origine. Una directory è copiata con tutto
  il suo contenuto. Copiare una voce su sé stessa o una directory nel
  proprio sottoalbero è rifiutato prima di scrivere qualsiasi cosa.
- `m` — spostare la voce selezionata, con la stessa richiesta di
  destinazione. Entro lo stesso volume lo spostamento è una rinomina
  atomica; tra volumi la voce viene copiata e poi l'origine rimossa.
- `r` — rinominare la voce selezionata sul posto: la riga è
  precompilata con il nome attuale.
- `d` — eliminare la voce selezionata dopo una conferma; solo `y`
  procede. Eliminare una directory rimuove tutto il suo contenuto, e la
  conferma lo dice.
- `M` — creare una directory nella directory elencata; il nome viene
  chiesto.
- `a` — modificare i bit di permesso della voce selezionata: una riga
  ottale precompilata con il modo attuale. Invio applica (solo il
  proprietario può cambiarlo — il kernel rifiuta chiunque altro), Esc
  annulla.
- `t` — marcare o smarcare la voce selezionata del pannello dei file e
  scendere di una riga; pressioni ripetute marcano quindi una serie.
  Le voci marcate portano un `*`.
- `T` — marcare per modello: un glob (`*`, `?`, `[...]`) confrontato
  con i nomi visibili; ogni corrispondenza si aggiunge all'insieme
  marcato.
- `i` — invertire le marcature sulle voci visibili.
- `C` — cancellare tutte le marcature.
- `u` — contare l'uso del disco sotto la directory attiva: file, byte
  e directory, percorsi in modo incrementale in secondo piano. `Esc`
  annulla conservando le cifre contate fin lì.
- `v` — appiattire il ramo sotto la directory attiva: un elenco di
  ogni file sottostante, riempito pagina per pagina (`Spazio` carica
  la successiva). Nella vista, `t`/`T`/`i`/`C` marcano le sue righe,
  `c`/`m`/`d` eseguono operazioni in blocco sull'insieme marcato e
  `Esc` torna ai pannelli. Le righe sono nominate relativamente al
  ramo appiattito.
- `.` — mostrare/nascondere le voci nascoste (nomi con punto) in entrambi
  i pannelli.
- `?` — mostrare questo aiuto sopra i pannelli; qualsiasi tasto lo chiude.
- `q` — uscire ripristinando il terminale.

Finché ci sono voci marcate, `c`, `m` e `d` agiscono sull'intero
insieme marcato invece che sulla selezione: `c`/`m` chiedono una
directory di destinazione esistente in cui le voci atterrano, e `d`
conferma l'eliminazione in blocco. Le voci sono elaborate in ordine di
marcatura; un errore non ferma mai il resto, il rapporto finale conta
ciò che è riuscito e una schermata di rapporto nomina ogni errore — un
blocco non è mai silenziosamente parziale. Le voci riuscite vengono
smarcate; gli errori restano marcati per riprovare.

Quando una copia o uno spostamento sovrascriverebbe un file esistente,
la sessione chiede file per file: `o` sovrascrive, `s` salta (un
origine saltata resta al suo posto) e `c` annulla i passi rimanenti —
in un blocco, annullare abbandona tutte le voci rimanenti —
ciò che è già stato applicato rimane, e il rapporto finale dice cosa è
successo. Un errore a metà copia rimuove la destinazione scritta a
metà e mostra l'errore del kernel; nulla si spaccia mai per una copia
completa. Ogni operazione è autorizzata dal kernel — un rifiuto
compare tale e quale sulla riga dei messaggi senza che nulla cambi.

La riga di stato mostra il percorso elencato, il numero di voci visibili,
l'ordinamento, i byte liberi/totali del volume sottostante (quando il
servizio di informazioni di sistema può riferirli), se le voci nascoste
sono visibili e — finché qualcosa è marcato — il numero di voci
marcate con il loro totale di byte. Un file il cui formato di
archiviazione non conserva la
data di modifica mostra `-` nella colonna della data.

La ricerca e i visualizzatori
testo/esadecimale/disassemblato arrivano nelle fasi successive del
piano dello strumento.

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

ls, cp, mv, rm, mkdir, chmod, du, df, find
