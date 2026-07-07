## NAME

wc — imprimir contagens de linhas, palavras e bytes de cada ficheiro

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

Conta, para cada `file`, as suas linhas (carateres de mudança de
linha), palavras e bytes, e imprime-as numa fila seguida do nome do
ficheiro. Sem `file`, ou quando `file` é `-`, lê-se a entrada padrão (e
não se imprime nome na forma sem operandos). Com mais de uma entrada,
imprime-se uma fila final `total` conforme `--total` selecionar.

Os seletores `-l`, `-w`, `-m`, `-c` e `-L` escolhem que contagens são
impressas; sem nenhum, imprimem-se as contagens de linhas, palavras e
bytes. As contagens aparecem sempre na ordem fixa: linhas, palavras,
carateres, bytes, largura máxima de linha. Uma palavra é uma sequência
máxima de carateres não brancos. `-m` conta carateres UTF-8 (um byte
que não é UTF-8 válido conta como byte mas não como caráter); `-L` mede
a largura de exibição de cada linha em colunas de terminal, com os
tabuladores a avançar para o próximo múltiplo de 8.

`--files0-from <file>` lê a lista de operandos, separada por NUL, de
`file` (`-` significa a entrada padrão); não pode ser combinado com
operandos `file`.

Uma entrada que não pode ser lida é reportada no erro padrão e a
execução continua com a entrada seguinte.

## OPTIONS

- `-c, --bytes` — imprimir a contagem de bytes.
- `-m, --chars` — imprimir a contagem de carateres.
- `-l, --lines` — imprimir a contagem de mudanças de linha.
- `-w, --words` — imprimir a contagem de palavras.
- `-L, --max-line-length` — imprimir a largura de exibição máxima de
  uma linha.
- `--files0-from <file>` — ler de `file` a lista de operandos separada
  por NUL (`-` lê-a da entrada padrão).
- `--total <when>` — quando imprimir a fila `total`: `auto` (a omissão:
  só com mais de uma entrada), `always`, `only` (só o total, sem
  rótulo) ou `never`.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `wc notes.txt` — imprimir as contagens de linhas, palavras e bytes de
  `notes.txt`.
- `wc -l a b` — imprimir a contagem de linhas de `a` e de `b`, depois o
  total.
- `wc -L table.txt` — imprimir a linha mais larga de `table.txt` em
  colunas de terminal.
- `wc -c --total=only a b` — imprimir apenas a contagem de bytes
  somada.

## EXIT STATUS

- `0` — todas as entradas foram contadas (ou a ajuda curta foi
  escrita).
- `1` — uma entrada não pôde ser lida, ou a saída não pôde ser
  entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `cat`
- `head`
- `man`
