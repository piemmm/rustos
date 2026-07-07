## NAME

reset — restaurar o terminal a um estado são

## SYNOPSIS

`reset`

## DESCRIPTION

Desfaz o estado que um programa de ecrã inteiro pode deixar ao falhar.
Primeiro restaura-se a disciplina de entrada ao valor interativo por
omissão (os carateres escritos voltam a ecoar). Depois escreve-se a
sequência de restauração: sair do ecrã alternativo, mostrar o cursor,
repor cores e atributos, repor a região de deslocamento e, por fim,
mover o cursor para o início e apagar o ecrã.

Quais operações são escritas decide-o o terminal indicado em `TERM`;
uma operação que o terminal não entende é omitida. Um terminal sem
controlos nenhuns (um `TERM` desconhecido degrada para a base «dumb»)
recebe apenas a restauração da disciplina de entrada.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `reset` — restaurar o terminal depois de um programa de ecrã inteiro
  ter falhado.

## EXIT STATUS

- `0` — o terminal foi restaurado.
- `1` — a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `TERM` — o terminal cuja sequência de restauração é escrita.
- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `clear`
- `man`
