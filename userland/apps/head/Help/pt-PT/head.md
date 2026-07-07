## NAME

head — mostrar a primeira parte de ficheiros

## SYNOPSIS

`head [option...] [file...]`

## DESCRIPTION

Imprime as primeiras 10 linhas de cada `file` na saída padrão. Com mais
de um `file`, cada parte é precedida por um cabeçalho `==> file <==`.
Sem `file`, ou quando `file` é `-`, lê-se a entrada padrão.

`-n` e `-c` mudam quanto é impresso: uma contagem simples imprime as
primeiras `num` linhas ou bytes; uma contagem escrita com `-` inicial
imprime tudo **exceto** as últimas `num` linhas ou bytes. Uma contagem
pode ter um sufixo multiplicador: `b` (512), `kB` (1000), `K` (1024),
`MB`, `M`, `GB`, `G`, e assim por diante para `T`, `P`, `E`, `Z`, `Y`,
`R`, `Q` (uma letra sozinha multiplica por potências de 1024; com `B`
por potências de 1000; com `iB` por potências de 1024).

A forma histórica de primeiro argumento `head -num` (com
multiplicadores `b`/`k`/`m` finais e letras `l`/`q`/`v`/`z` opcionais)
é aceite, como na ferramenta GNU.

Um ficheiro que não pode ser lido é reportado no erro padrão e a
execução continua com o ficheiro seguinte.

## OPTIONS

- `-c, --bytes <num>` — imprimir os primeiros `num` bytes de cada
  ficheiro; com `-` inicial, tudo menos os últimos `num` bytes.
- `-n, --lines <num>` — imprimir as primeiras `num` linhas de cada
  ficheiro; com `-` inicial, tudo menos as últimas `num` linhas.
- `-q, --quiet, --silent` — nunca imprimir os cabeçalhos
  `==> file <==`.
- `-v, --verbose` — imprimir sempre os cabeçalhos `==> file <==`.
- `-z, --zero-terminated` — as linhas são delimitadas por NUL em vez de
  por mudança de linha.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `head log.txt` — imprimir as primeiras 10 linhas de `log.txt`.
- `head -n 3 a b` — imprimir as primeiras 3 linhas de `a` e de `b`,
  cada uma sob o seu cabeçalho.
- `head -c 1K image` — imprimir os primeiros 1024 bytes de `image`.
- `head -n -1 notes` — imprimir `notes` sem a sua última linha.

## EXIT STATUS

- `0` — todos os ficheiros foram impressos (ou a ajuda curta foi
  escrita).
- `1` — um ficheiro não pôde ser lido, ou a saída não pôde ser
  entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `cat`
- `wc`
- `man`
