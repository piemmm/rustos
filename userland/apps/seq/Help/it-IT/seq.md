## NAME

seq — stampare una sequenza di numeri

## SYNOPSIS

`seq [-f format] [-s string] [-w] [primo [incremento]] ultimo`

## DESCRIPTION

Stampa i numeri da `primo` a `ultimo`, a passi di `incremento`, uno per
riga per impostazione predefinita. Un `primo` o un `incremento` omesso
vale 1 — anche quando `ultimo` è minore di `primo`, per cui `seq 5 1`
non stampa nulla. La sequenza termina quando aggiungere `incremento`
supererebbe `ultimo`.

I tre operandi sono letti come valori in virgola mobile; `incremento` è
di solito positivo quando `primo` è minore di `ultimo` e negativo nel
caso opposto, e non può essere zero. `ultimo` può essere `inf` per
contare senza fine. La precisione di stampa predefinita segue la
scrittura degli operandi (`seq 1 0.25 2` stampa due cifre decimali), e
le sequenze di interi sono generate in modo esatto, per quanto grandi
siano i numeri.

L'analisi delle opzioni si ferma al primo operando, e un numero negativo
iniziale è un operando, non un'opzione: `seq -5 5` conta da -5.

## OPTIONS

- `-f, --format <format>` — stampare ogni numero tramite il `<format>`
  in virgola mobile in stile printf (una sola direttiva `%` di tipo
  `e`, `f`, `g` o `a`, maiuscola o minuscola, con i consueti flag,
  larghezza e precisione). Non combinabile con `-w`.
- `-s, --separator <string>` — separare i numeri con `<string>` invece
  di un a capo. L'output termina comunque con un a capo.
- `-w, --equal-width` — riempire ogni numero con zeri iniziali fino a
  una larghezza comune. Non combinabile con `-f`.
- `-h, -?` — mostrare la guida breve di questo comando.
- `--` — terminare l'analisi delle opzioni; ogni argomento successivo è
  un operando.

## EXAMPLES

- `seq 5` — stampare da 1 a 5.
- `seq 2 5` — stampare da 2 a 5.
- `seq 1 2 10` — stampare i dispari da 1 a 9.
- `seq 5 -1 1` — contare all'indietro da 5 a 1.
- `seq -w 8 10` — stampare `08`, `09`, `10`.
- `seq -s , 3` — stampare `1,2,3`.
- `seq -f %.2f 3` — stampare `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — la sequenza (o la guida breve richiesta) è stata scritta.
- `1` — l'output ha smesso di accettare byte.
- `2` — la riga di comando non è stata compresa (opzione sconosciuta,
  numero non valido, incremento zero o formato errato).

## ENVIRONMENT

- `LANG` — la localizzazione preferita per la guida breve (un'etichetta
  BCP-47 come `fr-FR`).

## SEE ALSO

- `yes`
- `man`
