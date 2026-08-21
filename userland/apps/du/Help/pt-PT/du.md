## NAME

du — estimar o espaço em disco usado pelos ficheiros

## SYNOPSIS

`du [option...] [file...]`

## DESCRIPTION

Percorre cada operando `file` e imprime, por diretório (o mais
profundo primeiro), o armazenamento ocupado pela árvore abaixo dele,
como `size<TAB>path`. Sem `file`, é percorrido o diretório atual
(`.`). Um operando `file` que não seja um diretório é impresso
sozinho.

A medida predefinida é o armazenamento realmente alocado de cada nó,
tal como o sistema de ficheiros montado o reporta; ficheiros esparsos
ou comprimidos contam o que realmente ocupam. `--apparent-size` (ou
`-b`) mede antes os comprimentos aparentes em bytes. Os tamanhos são
impressos em blocos de 1024 bytes salvo se uma opção de unidade
escolher outra coisa; uma opção de unidade posterior substitui a
anterior, e as contagens de blocos arredondam para cima (um bloco
parcialmente usado é um bloco usado).

Um caminho ilegível é reportado no erro padrão e o percurso continua
com o resto; um diretório ilegível não contribui nada em vez de uma
soma parcial adivinhada.

Um ficheiro alcançado por mais de um nome é contado **uma só vez**, pelo
que o seu armazenamento não é reportado duas vezes; `-l` conta em vez
disso cada nome. `-x` (um só sistema de ficheiros) ainda não
está disponível; as variáveis de ambiente da família `DU_BLOCK_SIZE`
não são lidas — a escala é escolhida apenas pelas opções.

## OPTIONS

- `-a, --all` — reportar também cada ficheiro, não só os diretórios.
- `-s, --summarize` — reportar apenas o total de cada operando (em
  conflito com `-a` e `-d`).
- `-c, --total` — acrescentar uma linha de total geral etiquetada
  `total`.
- `-d, --max-depth <n>` — reportar diretórios até `n` níveis abaixo
  de um operando (`0` reporta só os operandos); os totais não mudam.
- `-S, --separate-dirs` — a linha de um diretório exclui os seus
  subdiretórios.
- `-l, --count-links` — contar um ficheiro com vários nomes uma vez
  por nome em vez de uma só.
- `--apparent-size` — medir comprimentos aparentes em bytes, não o
  armazenamento alocado.
- `-b, --bytes` — tamanho aparente em bytes simples
  (`--apparent-size` com tamanho de bloco 1).
- `-k` — blocos de 1024 bytes (a predefinição).
- `-m` — blocos de 1048576 bytes.
- `-h, --human-readable` — tamanhos legíveis em potências de 1024
  (`1.0K`, `23M`).
- `--si` — tamanhos legíveis em potências de 1000 (`1.0k`, `23M`).
- `-B, --block-size <size>` — reportar em blocos de `size` bytes
  (`512`, `1K`, `1MiB`, `1GB`, `human-readable`, `si`).
- `-0, --null` — terminar cada linha com NUL em vez de mudança de
  linha.
- `-?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `du` — a árvore do diretório atual, uma linha por diretório.
- `du -sh /Users/jo` — um total legível para `/Users/jo`.
- `du -a docs` — cada ficheiro e diretório sob `docs`.
- `du -d1 -c /Apps /Users` — o primeiro nível de cada armazém e
  depois um total geral.

## EXIT STATUS

- `0` — todos os operandos foram percorridos (ou a ajuda curta foi
  escrita).
- `1` — um caminho não pôde ser lido, ou a saída não pôde ser
  entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — o idioma preferido para a ajuda curta (uma etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

- `df`
- `ls`
- `man`
