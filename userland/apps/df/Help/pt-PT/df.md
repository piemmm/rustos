## NAME

df — reportar o uso de espaço dos sistemas de ficheiros

## SYNOPSIS

`df [option...] [file...]`

## DESCRIPTION

Reporta, uma linha por sistema de ficheiros montado, o tamanho do
volume, o espaço usado, o espaço disponível, a percentagem usada e o
ponto de montagem. Com operandos `file` reporta em vez disso o sistema
de ficheiros que contém cada operando (uma linha por sistema de
ficheiros, cubra quantos operandos cobrir).

Os números vêm da listagem de montagens da API de informação do
sistema, tal como cada controlador de sistema de ficheiros montado
reporta a sua própria contabilidade. Por predefinição, o relatório
oculta as montagens sem capacidade própria (as ligações de vista
sintéticas do sistema) e montagens adicionais de um volume já
listado; `-a` mostra tudo, e o número de entradas ocultas é anotado no
fluxo de informação padrão (fd 3), nunca na tabela.

Os tamanhos são impressos em blocos de 1024 bytes salvo se uma opção
de unidade escolher outra coisa; uma opção de unidade posterior
substitui a anterior, e as contagens de blocos arredondam para cima.
Um sistema de ficheiros cujo formato aloca inodes a pedido reporta
valores de inodes a zero com `-i` — a resposta honesta «não
rastreado».

Um operando `file` que não existe, ou que é um caminho relativo (os
pontos de montagem são absolutos; o `df` nunca adivinha uma
resolução), é reportado no erro padrão e o relatório continua com o
resto. As opções GNU `--output`, `--sync` e `--no-sync` ainda não
estão disponíveis.

## OPTIONS

- `-a, --all` — incluir as montagens sem capacidade e duplicadas que
  a predefinição oculta.
- `-T, --print-type` — acrescentar a coluna do tipo de sistema de
  ficheiros.
- `-t, --type <type>` — reportar apenas sistemas de ficheiros do tipo
  `type` (repetível).
- `-x, --exclude-type <type>` — omitir sistemas de ficheiros do tipo
  `type` (repetível).
- `-i, --inodes` — reportar contagens de inodes em vez do uso de
  blocos.
- `-P, --portability` — o formato portátil POSIX (cabeçalhos
  `1024-blocks` e `Capacity`).
- `-l, --local` — restringir o relatório a sistemas de ficheiros
  locais (hoje todas as montagens RustOS: nada é filtrado).
- `--total` — acrescentar uma linha etiquetada `total` que soma os
  valores mostrados.
- `-k` — blocos de 1024 bytes (a predefinição).
- `-h, --human-readable` — tamanhos legíveis em potências de 1024
  (`1.0K`, `23M`).
- `-H, --si` — tamanhos legíveis em potências de 1000 (`1.0k`,
  `23M`).
- `-B, --block-size <size>` — reportar em blocos de `size` bytes
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `df` — o uso de cada volume real em blocos de 1024 bytes.
- `df -h` — o mesmo, em tamanhos legíveis.
- `df /Users/jo` — o sistema de ficheiros que contém `/Users/jo`.
- `df -aT` — cada montagem, com o seu tipo de sistema de ficheiros.
- `df --total -k` — os volumes mais uma linha `total` somada.

## EXIT STATUS

- `0` — o relatório cobriu tudo o que foi pedido (ou a ajuda curta
  foi escrita).
- `1` — um operando não pôde ser reportado, os filtros não deixaram
  nada, ou a consulta ou a saída falharam.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — o idioma preferido para a ajuda curta (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `du`
- `mount`
- `man`
