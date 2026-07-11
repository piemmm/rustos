## NAME

lsusb — elencare i dispositivi USB rilevati

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Elenca, una riga per ogni dispositivo USB rilevato, i numeri di bus e
di dispositivo del dispositivo, il suo identificatore
`vendor:product` e i nomi del produttore e del prodotto. L'inventario
è l'albero hardware — l'unico inventario dei dispositivi del sistema —
letto attraverso l'API di informazioni di sistema, che richiede la
capacità `CAP_SYSINFO_HW`; un rifiuto viene segnalato sull'errore
standard e al suo posto non viene elencato nulla.

I nomi provengono dall'istantanea verificata del database pubblico
degli identificatori USB che questo comando include nel proprio
pacchetto. Un'identità che il database non nomina mostra solo la sua
forma numerica `ID vvvv:pppp`, mai inventata, e il numero di tali
dispositivi viene annotato sul flusso di informazioni standard (fd 3).
Se la tabella inclusa manca o non supera la convalida, l'elenco
degrada a identificatori nudi con la ragione sull'errore standard —
l'inventario stesso viene comunque elencato.

RustOS non ha il registro Linux dei numeri di bus/dispositivo: i
numeri di bus e di dispositivo sono piccoli ordinali che partono da 1
sull'inventario corrente (i bus in ordine di rilevamento, i
dispositivi in ordine di elenco su ciascun bus), stabili finché la
topologia non cambia, e `-s` seleziona quei numeri mostrati (una
divergenza deliberata e documentata rispetto a `lsusb` di Linux).
L'inventario registra una voce per *interfaccia*: le interfacce di uno
stesso dispositivo fisico vengono raggruppate in base all'indirizzo di
dispositivo riportato dal controller host, cosicché un dispositivo con
più interfacce compare una sola volta.

## OPTIONS

- `-v` — dopo ogni dispositivo, elencare la classe, la sottoclasse e
  il protocollo di ciascuna delle sue interfacce (`bInterfaceClass`,
  `bInterfaceSubClass`, `bInterfaceProtocol`) con i nomi delle tabelle
  delle classi USB.
- `-t` — mostrare come un albero i bus, i loro dispositivi e le classi
  di interfaccia di ciascun dispositivo.
- `-d [<vendor>]:[<product>]` — elencare solo i dispositivi che
  corrispondono agli identificatori produttore/prodotto dati (hex);
  una metà omessa corrisponde a qualsiasi.
- `-s [[<bus>]:][<devnum>]` — elencare solo i dispositivi che
  corrispondono ai numeri di bus e/o di dispositivo dati (decimale),
  come mostrati nell'elenco; un valore senza due punti è un numero di
  dispositivo da solo.
- `-?, --help` — mostrare la guida breve di questo comando.

## EXAMPLES

- `lsusb` — ogni dispositivo USB rilevato, con i nomi.
- `lsusb -v` — lo stesso, con l'identità di classe di ogni
  interfaccia.
- `lsusb -s 2:` — ogni dispositivo sul bus 2.
- `lsusb -d 046d:` — ogni dispositivo del produttore `046d`
  (Logitech).
- `lsusb -t` — i dispositivi nella loro topologia di bus.

## EXIT STATUS

- `0` — l'elenco (o la guida breve) è stato scritto.
- `1` — l'interrogazione dell'albero hardware è stata rifiutata o è
  fallita, oppure l'output non ha potuto essere scritto.
- `2` — la riga di comando non è stata compresa.

## ENVIRONMENT

- `LANG` — la locale preferita per la guida breve (un'etichetta
  BCP-47 come `it-IT`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
