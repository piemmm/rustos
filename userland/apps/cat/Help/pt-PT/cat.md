## NAME

cat — concatenar ficheiros para a saída padrão

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Lê cada operando de ficheiro por ordem e escreve os seus bytes na saída
padrão. O operando `-` designa a entrada padrão e, sem operando, a
entrada padrão é a única fonte.

Um operando também pode ser uma referência de recurso tipada como
`sys:random`: é aberta através do resolvedor de recursos do sistema
(verificado por capacidades) em vez do sistema de ficheiros — `cat
sys:random` emite bytes aleatórios. Uma referência malformada num espaço
de nomes registado é um erro, nunca um nome de ficheiro.

Com `-n`, as linhas de saída são numeradas continuamente através de
todas as fontes, pelo que uma linha que atravessa duas fontes é
numerada exatamente uma vez, quando o seu primeiro byte aparece. `-b`
numera apenas as linhas não vazias e sobrepõe-se a `-n`. `-s` suprime
linhas em branco adjacentes repetidas, e uma linha suprimida não é
escrita nem numerada.

As opções de marcação tornam visíveis os bytes invisíveis: `-E` imprime
`$` antes de cada mudança de linha, `-T` imprime TAB como `^I` e `-v`
imprime os outros bytes de controlo como `^X` e os bytes não-ASCII em
notação `M-`. `-e`, `-t` e `-A` são as combinações habituais `-vE`,
`-vT` e `-vET`.

Uma fonte que não pode ser lida para o comando antes de qualquer fonte
posterior ser tocada; os bytes já escritos ficam escritos.

## OPTIONS

- `-A, --show-all` — equivalente a `-vET`.
- `-b, --number-nonblank` — numerar as linhas de saída não vazias;
  sobrepõe-se a `-n`.
- `-e` — equivalente a `-vE`.
- `-E, --show-ends` — imprimir `$` no fim de cada linha.
- `-n, --number` — numerar as linhas de saída, continuamente através de
  todas as fontes.
- `-s, --squeeze-blank` — suprimir linhas em branco adjacentes
  repetidas.
- `-t` — equivalente a `-vT`.
- `-T, --show-tabs` — imprimir os carateres TAB como `^I`.
- `-u` — aceite e ignorado; a saída já não é armazenada em buffer.
- `-v, --show-nonprinting` — usar as notações `^` e `M-` para os bytes
  de controlo e não-ASCII, exceto a mudança de linha e o TAB.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `cat notes.txt` — escrever `notes.txt` na saída padrão.
- `cat a.txt - b.txt` — escrever `a.txt`, depois a entrada padrão,
  depois `b.txt`.
- `cat -n log.txt` — numerar todas as linhas de saída.
- `cat -bs draft.txt` — numerar as linhas não vazias e comprimir as
  sequências em branco.
- `cat -A config.txt` — tornar visíveis os fins de linha, os TAB e os
  bytes de controlo.
- `cat -- -n` — escrever o ficheiro chamado `-n`.

## EXIT STATUS

- `0` — todas as fontes foram escritas.
- `1` — uma fonte não pôde ser lida, ou a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `ls`
- `man`
