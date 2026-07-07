## NAME

mkdir — criar diretórios

## SYNOPSIS

`mkdir [-pv] [--] directory...`

## DESCRIPTION

Cria cada operando de diretório, por ordem. Sem `-p`, o pai de cada
operando tem de existir já e o próprio operando não pode existir; a
primeira falha para a execução antes de qualquer operando posterior.

Com `-p`, cada antepassado em falta é criado primeiro, do mais exterior
para dentro, e um operando (ou antepassado) que já exista como
diretório não é um erro. Um antepassado que exista como ficheiro
continua a falhar: nada é jamais substituído em silêncio.

O `-m`/`--mode` do `mkdir` GNU ainda não é aceite: os diretórios são
criados com o modo por omissão do sistema de ficheiros até a facilidade
de definição de modos chegar, e o interruptor chegará com ela em vez de
ser ignorado. `--` termina a análise de opções: cada argumento
posterior é um caminho.

## OPTIONS

- `-p, --parents` — criar os diretórios pai em falta; um operando que
  já seja um diretório não é um erro.
- `-v, --verbose` — reportar cada diretório criado como
  `mkdir: created directory 'dir'`.
- `-h, -?` — mostrar a ajuda curta deste próprio comando (também
  `--help`).

## EXAMPLES

- `mkdir Notes` — criar um diretório no diretório atual.
- `mkdir -p Projects/os/build` — criar toda a cadeia, saltando as
  partes que já existem.
- `mkdir -pv Home:/tools/bin` — criar sob uma raiz de alias, reportando
  cada diretório novo.

## EXIT STATUS

- `0` — todos os diretórios foram criados (ou, com `-p`, já existiam).
- `1` — uma falha do sistema de ficheiros ou da saída; a razão é
  impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

rmdir, rm, ls
