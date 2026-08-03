## NAME

mdadm — ispezionare e amministrare gli array RAID

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Ispeziona e amministra gli array RAID software che il compositore di
array assembla dai dispositivi membri. L'inventario di array e
dispositivi è letto tramite l'API di informazioni di sistema — la stessa
interfaccia, allo stesso livello `CAP_SYSINFO_HW` con cui si legge
l'albero dell'hardware. Le mutazioni di creazione, aggiunta, rimozione e
arresto sono inviate al punto di controllo del compositore, che verifica
che il chiamante possieda `CAP_STORAGE_ADMIN` prima di agire. Un rifiuto
è segnalato sull'errore standard con un codice di uscita diverso da zero;
nulla è inventato e non si presume alcuna autorità.

Si indica esattamente una modalità per invocazione.

TAIRiX non ha `/dev`, quindi i due nomi che Linux mdadm scrive come file
di dispositivo si scrivono qui in modo diverso — una divergenza
deliberata e documentata:

- Un dispositivo è nominato dall'identificatore del suo nodo nell'albero
  dell'hardware, scritto `node:<id>`, lo stesso nome che mostrano i
  rapporti. Ogni altra grafia è rifiutata anziché indovinata.
- Un array è nominato dalla sua identità a 128 bit in esadecimale. Si
  accetta l'identità completa di 32 cifre, così come qualsiasi prefisso
  che nomini esattamente un array; un prefisso che corrisponde a più di
  un array è rifiutato anziché indovinare quale si intendesse.

TAIRiX compone i livelli RAID 0, 1, 5, 6, 10 e la tripla parità. Non ha
RAID4, quindi `--level=4` è rifiutato con questa motivazione.

Un conciso contesto consultivo — un array degradato, o dispositivi vuoti
non mostrati nella vista degli array — è scritto sul flusso di
informazioni standard (fd 3). È facoltativo e non cambia mai l'output
principale.

## OPTIONS

- `-C, --create` — creare un array sui dispositivi nominati e stampare
  l'identità che il compositore gli assegna.
- `-D, --detail` — riportare identità, livello, salute, conteggi dei
  dispositivi, geometria e ogni posizione di ricostruzione o verifica di
  ciascun array. Senza operando di array, riportare ogni array.
- `-E, --examine` — elencare ogni dispositivo che il compositore
  detiene: i membri degli array con il loro alloggiamento e stato, e i
  dispositivi vuoti non affiliati su cui si può creare un nuovo array.
- `-a, --add` — ammettere un dispositivo vuoto in un alloggiamento
  assente di un array e ricostruirlo.
- `-r, --remove` — ritirare un dispositivo membro da un array.
- `-S, --stop` — arrestare un array attivo e liberare i suoi membri.
- `-l, --level=<level>` — il livello da creare: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, o
  `tp`/`raid-tp` per la tripla parità.
- `-n, --raid-devices=<count>` — il numero di alloggiamenti membro da
  creare; deve essere uguale al numero di operandi di dispositivo.
- `-c, --chunk=<blocks>` — l'unità di striping in blocchi logici; valida
  solo per un livello con striping.
- `-h, -?, --help` — mostrare l'aiuto proprio di questo comando.
- `-V, --version` — stampare la versione e uscire.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — creare un array RAID5 su tre dispositivi.
- `mdadm --detail` — riportare ogni array.
- `mdadm --examine` — elencare ogni dispositivo, membri e vuoti.
- `mdadm --add 3f2a node:14` — aggiungere un dispositivo all'array la cui identità inizia con `3f2a`.
- `mdadm --stop 3f2a` — arrestare quell'array.

## EXIT STATUS

- `0` — la richiesta è riuscita (o l'aiuto è stato scritto).
- `1` — una capacità è stata negata, un nome non si è risolto, il
  compositore ha rifiutato la richiesta, o l'output non ha potuto essere
  scritto.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per questo aiuto (un'etichetta BCP-47 come
  `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
