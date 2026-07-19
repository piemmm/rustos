## NAME

lspci — elencare i dispositivi PCI/PCIe rilevati

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Elenca, una riga per ogni funzione PCI/PCIe rilevata, un piccolo numero
di elenco, la sua classe e i nomi
del produttore e del dispositivo. L'inventario è l'albero hardware —
l'unico inventario dei dispositivi del sistema — letto tramite l'API
di informazioni di sistema, che richiede la capability
`CAP_SYSINFO_HW`; un rifiuto viene riportato sull'errore standard e al
suo posto non viene elencato nulla.

I nomi provengono dall'istantanea verificata della base pubblica di
identificatori PCI che questo comando include nel proprio pacchetto.
Un'identità che la base non nomina è mostrata in forma numerica
(`Vendor 8086`, `Device 2922`, `Class 0106`), mai inventata, e il
numero di tali dispositivi è annotato sul flusso informativo standard
(fd 3). Se la tabella inclusa manca o non supera la validazione,
l'elenco degrada agli identificatori numerici con la ragione
sull'errore standard — l'inventario stesso viene comunque elencato.

TAIRiX non registra un indirizzo PCI `bus:device.function`. Invece a
ogni dispositivo elencato viene assegnato un piccolo numero stabile
nell'ordine del bus, mostrato come `#<n>`, e `-s` seleziona quel numero
(una divergenza deliberata e documentata rispetto a `lspci` di Linux).
Quel numero *non* è l'identificatore di nodo interno dell'albero
hardware, che proviene da uno spazio riservato e può essere un valore
grande e privo di significato. La vista `-k` (driver del kernel) non è ancora offerta: il
sistema non pubblica record di associazione dei driver, e `lspci`
riporta solo ciò che il sistema registra davvero.

## OPTIONS

- `-n` — solo identificatori numerici: il codice di classe e
  `vendor:device` in esadecimale.
- `-nn` — i nomi seguiti dagli identificatori numerici tra parentesi
  quadre.
- `-v` — dopo ogni funzione, elencare le risorse dichiarate dal suo
  nodo (finestre MMIO, linee IRQ, porte di I/O, vincoli DMA) — le
  richieste di concessione registrate, non lo stato in tempo reale.
- `-t` — rappresentare le funzioni come albero sotto i bus genitori;
  ogni riga di bus intermedio nomina la sua classe e la sua identità
  di chiave di corrispondenza e, con `-v` (`-tv`), mostra anche le sue
  risorse dichiarate.
- `-d [<vendor>]:[<device>]` — elencare solo le funzioni che
  corrispondono agli identificatori dati (esadecimale); una metà
  omessa corrisponde a qualsiasi valore.
- `-s <node>` — elencare solo la funzione con il numero di elenco
  dato (il `#<n>` decimale mostrato nell'elenco).
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `lspci` — ogni funzione PCI rilevata, con i nomi.
- `lspci -nn` — lo stesso, con accanto gli identificatori numerici.
- `lspci -v -s 7` — la riga del dispositivo `#7` più le risorse dichiarate.
- `lspci -d 1af4:` — ogni funzione del produttore `1af4` (virtio).
- `lspci -t` — le funzioni nella loro topologia di bus.

## EXIT STATUS

- `0` — l'elenco (o la guida breve) è stato scritto.
- `1` — l'interrogazione dell'albero hardware è stata rifiutata o è
  fallita, oppure l'output non è stato scritto.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un tag
  BCP-47 come `it-IT`).

## SEE ALSO

- `sysinfo`
- `man`
