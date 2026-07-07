## NAME

true — não fazer nada, com sucesso

## SYNOPSIS

`true [ignored arguments]`

## DESCRIPTION

Termina com o estado `0`, ignorando todos os argumentos. Os scripts
usam-no onde é preciso um comando que tem sempre sucesso — como comando
de preenchimento, condição sempre verdadeira ou corpo de um ciclo.

Só um **primeiro** argumento `-h`, `-?` ou `--help` é honrado (a posição
em que o `true` do GNU honra `--help`); em qualquer posição posterior
esses símbolos são ignorados como tudo o resto.

## OPTIONS

- `-h, -?` — (apenas como primeiro argumento) mostrar a ajuda curta
  deste próprio comando.

## EXAMPLES

- `true` — ter sucesso.
- `while true; do …; done` — repetir até ser interrompido.

## EXIT STATUS

- `0` — sempre (todo o propósito da ferramenta).
- `1` — não foi possível escrever uma ajuda curta pedida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `false`
- `man`
