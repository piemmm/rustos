## NAME

mv — mover (renomear) ficheiros e diretórios

## SYNOPSIS

`mv [-finvT] [-t dir] [--] source... dest`

## DESCRIPTION

Move cada operando de origem para um destino. Com uma única origem e
um destino que não nomeia um diretório, a origem é renomeada para esse
caminho exato. Quando o destino nomeia um diretório existente — e
sempre que há mais de uma origem — cada origem é movida *para dentro*
desse diretório sob o seu próprio nome base.

Um movimento dentro de um volume é um renomeamento atómico que
preserva a identidade do nó. Um movimento cuja origem e destino estão
em volumes diferentes não pode ser atómico: recorre a copiar a origem
para o destino e depois remover a origem (os diretórios são
reproduzidos recursivamente).

Um destino existente é sobrescrito por omissão, saltado com `-n`, e
perguntado no fluxo de erro padrão com `-i` (uma pergunta recusada
salta esse movimento sem erro; uma resposta ilegível nunca é tratada
como consentimento). A primeira falha para a execução antes de
qualquer operando posterior. `--` termina a análise de opções: cada
argumento posterior é um caminho.

## OPTIONS

- `-f, --force` — remover um destino que bloqueia e tentar o
  renomeamento de novo; nunca perguntar. O mais tardio de `-f`, `-i` e
  `-n` vence.
- `-i, --interactive` — perguntar antes de sobrescrever um destino
  existente; só uma resposta a começar por `y`/`Y` consente.
- `-n, --no-clobber` — nunca sobrescrever um destino existente.
- `-v, --verbose` — reportar cada movimento como
  `renamed 'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — mover todas as origens para
  `dir`, que tem de ser um diretório existente. O valor segue colado
  (`-tdir`, `--target-directory=dir`) ou como o argumento seguinte.
- `-T, --no-target-directory` — tratar o destino como um ficheiro
  normal; permite-se exatamente uma origem. Não pode ser combinado com
  `-t`.
- `-h, -?, --help` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `mv draft.txt final.txt` — renomear um ficheiro.
- `mv -v a.txt b.txt Archive` — mover ambos os ficheiros para
  `Archive`, reportando cada movimento.
- `mv -n new.cfg current.cfg` — instalar um ficheiro apenas se o
  destino ainda não existir.

## EXIT STATUS

- `0` — todos os movimentos tiveram êxito (um salto por `-n` e uma
  pergunta `-i` recusada não são falhas).
- `1` — uma falha do sistema de ficheiros, do diálogo ou da saída; a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `cp`
- `ls`
- `rm`
