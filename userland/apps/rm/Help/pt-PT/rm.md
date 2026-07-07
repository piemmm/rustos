## NAME

rm — remover ficheiros e diretórios

## SYNOPSIS

`rm [-dfiIrRv] [--] file...`

## DESCRIPTION

Remove cada operando de ficheiro, por ordem. Um operando que não é
diretório é desligado; um operando de diretório só é removido com `-r`
(que remove o seu conteúdo em profundidade primeiro e depois o próprio
diretório) ou, quando vazio, com `-d`.

Com `-f`, um operando que não existe é saltado em silêncio e nunca se
faz pergunta alguma. `-i` pergunta no fluxo de erro padrão antes de
cada remoção e antes de descer para um diretório; `-I` pergunta uma vez
no início, antes de remover mais de três operandos ou antes de uma
remoção recursiva. Uma pergunta recusada salta o objeto (ou toda a
execução, no caso de `-I`) sem erro; uma resposta ilegível nunca é
tratada como consentimento. O mais tardio de `-f`, `-i` e `-I` vence.

O operando `/` é recusado com `--preserve-root`, a omissão. A primeira
falha para a execução antes de qualquer operando posterior. `--`
termina a análise de opções: cada argumento posterior é um caminho.

## OPTIONS

- `-r, -R, --recursive` — remover diretórios e o seu conteúdo.
- `-f, --force` — ignorar operandos que não existem; nunca perguntar.
- `-d, --dir` — remover diretórios vazios.
- `-i, --interactive` — perguntar antes de cada remoção; só uma
  resposta a começar por `y`/`Y` consente.
- `-I` — perguntar uma vez antes de remover mais de três operandos ou
  antes de uma remoção recursiva.
- `-v, --verbose` — reportar cada remoção como `removed 'file'`.
- `--preserve-root` — recusar remover `/` (a omissão).
- `--no-preserve-root` — permitir remover `/`.
- `-h, -?, --help` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `rm notes.txt` — remover um ficheiro.
- `rm -r Scratch` — remover a árvore `Scratch` e tudo o que contém.
- `rm -I a b c d` — perguntar uma vez e remover os quatro ficheiros com
  um `y`.

## EXIT STATUS

- `0` — todas as remoções tiveram êxito (uma pergunta recusada e um
  salto por `-f` não são falhas).
- `1` — uma falha do sistema de ficheiros, do diálogo ou da saída; a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `cp`
- `ls`
- `mv`
