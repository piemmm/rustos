## NAME

false — não fazer nada, sem sucesso

## SYNOPSIS

`false [ignored arguments]`

## DESCRIPTION

Termina com o estado `1`, ignorando todos os argumentos. Os scripts
usam-no onde é preciso um comando que falha sempre — como condição
sempre falsa ou uma falha deliberada.

Só um **primeiro** argumento `-h`, `-?` ou `--help` é honrado (a posição
em que o `false` do GNU honra `--help`); em qualquer posição posterior
esses símbolos são ignorados como tudo o resto. Ao contrário do GNU
`false --help`, que mesmo assim termina com `1`, uma ajuda curta servida
termina aqui com `0` — a convenção de ajuda curta do RustOS.

## OPTIONS

- `-h, -?` — (apenas como primeiro argumento) mostrar a ajuda curta
  deste próprio comando.

## EXAMPLES

- `false` — falhar.
- `until false; do …; done` — executar o corpo uma vez (a condição é
  sempre falsa).

## EXIT STATUS

- `1` — sempre (todo o propósito da ferramenta).
- `0` — uma ajuda curta pedida foi servida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `true`
- `man`
