## NAME

rmdir — remover diretórios vazios

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

Remove cada operando de diretório, por ordem. Só um **diretório vazio**
é removido: o próprio sistema de ficheiros recusa um ficheiro (ou
qualquer não-diretório) e um diretório povoado, atomicamente, pelo que
nada mais pode jamais ser desligado no seu lugar. Use `rm` para
ficheiros e `rm -r` para árvores povoadas.

Com `-p`, os antepassados de cada operando são removidos também, do
mais interior para fora: `rmdir -p a/b/c` remove `a/b/c`, depois `a/b`
e depois `a`. A raiz nua de um caminho (`/` ou uma raiz de alias como
`Home:/`) nunca é objeto de remoção.

Com `--ignore-fail-on-non-empty`, a recusa «diretório não vazio» não é
um erro — o operando (ou o percurso de `-p`) simplesmente para aí.
Nenhuma outra recusa é tolerada. A primeira falha genuína para a
execução antes de qualquer operando posterior. `--` termina a análise
de opções: cada argumento posterior é um caminho.

## OPTIONS

- `-p, --parents` — remover também os antepassados de cada operando, do
  mais interior para fora.
- `-v, --verbose` — reportar cada tentativa de remoção como
  `rmdir: removing directory, 'dir'`.
- `--ignore-fail-on-non-empty` — um diretório que não esteja vazio não
  é um erro; com `-p`, a subida para aí.
- `-h, -?` — mostrar a ajuda curta deste próprio comando (também
  `--help`).

## EXAMPLES

- `rmdir Scratch` — remover um diretório vazio.
- `rmdir -p Projects/os/build` — remover a cadeia, do interior para
  fora.
- `rmdir -p --ignore-fail-on-non-empty a/b` — remover `a/b`, e também
  `a` se isso o deixar vazio.

## EXIT STATUS

- `0` — todas as remoções tiveram êxito (uma recusa tolerada por
  `--ignore-fail-on-non-empty` não é uma falha).
- `1` — uma falha do sistema de ficheiros ou da saída; a razão é
  impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

mkdir, rm, ls
