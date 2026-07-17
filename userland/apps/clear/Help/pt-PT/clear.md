## NAME

clear — limpar o ecrã do terminal

## SYNOPSIS

`clear [-x]`

## DESCRIPTION

Escreve a sequência que move o cursor para o canto superior esquerdo e
apaga todo o ecrã, deixando-o vazio. Qual sequência é escrita decide-o o
terminal indicado em `TERM`; um terminal que não sabe limpar (um `TERM`
desconhecido degrada para a base «dumb») faz o comando falhar em vez de
imprimir bytes que o terminal mostraria como lixo.

As consolas TAIRiX não guardam histórico de deslocamento, pelo que não há
histórico para limpar: `-x` (a opção GNU que preserva o histórico) é
aceite por compatibilidade de scripts e não muda nada.

## OPTIONS

- `-x` — aceite por compatibilidade com o GNU; uma consola TAIRiX não
  guarda histórico, pelo que a saída é idêntica com e sem ela.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `clear` — limpar o ecrã.

## EXIT STATUS

- `0` — a sequência de limpeza foi escrita.
- `1` — o terminal não sabe limpar, ou a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `TERM` — o terminal cuja sequência de limpeza é escrita.
- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `reset`
- `man`
