## NAME

tail — mostrar a última parte dos ficheiros

## SYNOPSIS

`tail [option...] [file...]`

## DESCRIPTION

Imprime as últimas 10 linhas de cada `file` na saída padrão. Com mais do
que um `file`, cada parte é precedida de um cabeçalho `==> file <==`. Sem
`file`, ou quando `file` é `-`, é lida a entrada padrão.

`-n` e `-c` alteram quanto é impresso: uma contagem simples (ou escrita
com um `-` inicial) imprime as últimas `num` linhas ou bytes; uma
contagem escrita com um `+` inicial imprime tudo **a partir** da linha ou
do byte `num` (a contar de 1) até ao fim. Uma contagem pode ter um
sufixo multiplicador: `b` (512), `kB` (1000), `K` (1024), `MB`, `M`,
`GB`, `G`, e assim por diante para `T`, `P`, `E`, `Z`, `Y`, `R`, `Q` (uma
letra sozinha multiplica por potências de 1024; com `B` por potências de
1000; com `iB` por potências de 1024).

A forma histórica em primeiro argumento `tail -num` / `tail +num` (com uma
letra final `b`/`c`/`l` opcional) é aceite, tal como na ferramenta GNU.

O modo de seguimento (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`) ainda não está disponível e
é reportado como uma opção desconhecida: precisa de uma fonte de
despertar em mudanças de ficheiro que o sistema ainda não expõe, e não é
fornecida uma espera ativa no seu lugar.

Quando conteúdo inicial não é mostrado, é escrito um registo informativo
no fluxo de informação padrão (fd 3); nunca altera a saída nem o estado
de saída. Um ficheiro que não pode ser lido é reportado na saída de erro
e a execução continua com o ficheiro seguinte.

## OPTIONS

- `-c, --bytes <num>` — imprimir os últimos `num` bytes de cada ficheiro;
  com um `+` inicial, tudo a partir do byte `num`.
- `-n, --lines <num>` — imprimir as últimas `num` linhas de cada
  ficheiro; com um `+` inicial, tudo a partir da linha `num`.
- `-q, --quiet, --silent` — nunca imprimir os cabeçalhos `==> file <==`.
- `-v, --verbose` — imprimir sempre os cabeçalhos `==> file <==`.
- `-z, --zero-terminated` — as linhas são delimitadas por NUL em vez da
  mudança de linha.
- `-h, -?` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `tail log.txt` — imprimir as últimas 10 linhas de `log.txt`.
- `tail -n 3 a b` — imprimir as últimas 3 linhas de `a` e de `b`, cada
  uma sob o seu cabeçalho.
- `tail -c 1K image` — imprimir os últimos 1024 bytes de `image`.
- `tail -n +5 notes` — imprimir `notes` a partir da sua 5.ª linha.

## EXIT STATUS

- `0` — cada ficheiro foi impresso (ou a ajuda breve foi escrita).
- `1` — um ficheiro não pôde ser lido, ou a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `head`
- `cat`
- `wc`
- `man`
