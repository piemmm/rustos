## NAME

cp — copiar ficheiros e diretórios

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

Copia cada operando de origem para um destino. Com uma única origem e
um destino que não nomeia um diretório, a origem é copiada para esse
caminho exato. Quando o destino nomeia um diretório existente — e
sempre que há mais de uma origem — cada origem é copiada *para dentro*
desse diretório sob o seu próprio nome base.

Uma origem que é diretório só é copiada com `-r`, que reproduz toda a
subárvore; sem `-r`, um operando de diretório é recusado. Um ficheiro
de destino existente é sobrescrito por omissão, saltado com `-n`, e
perguntado no fluxo de erro padrão com `-i` (uma pergunta recusada
salta essa cópia sem erro; uma resposta ilegível nunca é tratada como
consentimento).

A primeira falha para a execução antes de qualquer operando posterior.
`--` termina a análise de opções: cada argumento posterior é um
caminho.

## OPTIONS

- `-r, -R, --recursive` — copiar diretórios e o seu conteúdo.
- `-f, --force` — quando um ficheiro de destino não pode ser criado,
  removê-lo e tentar a cópia uma vez mais.
- `-i, --interactive` — perguntar antes de sobrescrever um ficheiro
  existente; só uma resposta a começar por `y`/`Y` consente.
- `-n, --no-clobber` — nunca sobrescrever um ficheiro existente. O
  mais tardio de `-i` e `-n` vence.
- `-l, --link` — dar ao destino um segundo nome do nó da origem em vez de
  copiar os seus bytes, de modo que os dois nomes não possam divergir numa
  escrita posterior. Uma origem que é directório continua a exigir `-r`.
- `-s, --symbolic-link` — criar uma ligação simbólica que nomeia a origem
  em vez de a copiar.
- `-P, --no-dereference` — reproduzir uma origem que é ligação simbólica
  como uma ligação que guarda o mesmo destino, literalmente, em vez de
  copiar aquilo que nomeia (assim uma ligação relativa ou pendente
  sobrevive à cópia). Sem ela, uma ligação de origem é seguida.
- `--preserve=links` — duas origens que nomeiam um só nó obtêm dois nomes
  no destino em vez de duas cópias, pelo que a cópia não duplica o
  armazenamento em silêncio.
- `-d` — `-P` e `--preserve=links` juntas, como na ferramenta GNU.
- `-v, --verbose` — reportar cada cópia como `'source' -> 'dest'`.
- `-t dir, --target-directory=dir` — copiar todas as origens para
  `dir`, que tem de ser um diretório existente. O valor segue colado
  (`-tdir`, `--target-directory=dir`) ou como o argumento seguinte.
- `-T, --no-target-directory` — tratar o destino como um ficheiro
  normal; permite-se exatamente uma origem. Não pode ser combinado com
  `-t`.
- `-h, -?, --help` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `cp notes.txt backup.txt` — copiar um ficheiro para um novo nome.
- `cp -r Projects Archive` — reproduzir a árvore `Projects` dentro de
  `Archive` (ou como `Archive` se este não existir).
- `cp -v -t Backup a.txt b.txt` — copiar ambos os ficheiros para
  `Backup`, reportando cada cópia.

## EXIT STATUS

- `0` — todas as cópias tiveram êxito (um salto por `-n` e uma
  pergunta `-i` recusada não são falhas).
- `1` — uma falha do sistema de ficheiros, do diálogo ou da saída; a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `ls`
- `mv`
- `rm`
