## NAME

vim — o editor de texto modal

## SYNOPSIS

`vim [-R] [+num | + | +/pattern] [--] [file ...]`

## DESCRIPTION

Edita ficheiros de texto com o conjunto de comandos modal do conhecido
editor vim. A sessão começa em modo normal: as teclas são comandos, e
`i` (ou `a`, `o` e as suas variantes) entra no modo de inserção, onde
escrever se torna texto. `Esc` regressa ao modo normal. `:q` sai;
`:wq` (ou `ZZ`) grava e sai.

Podem ser nomeados vários ficheiros; a sessão abre o primeiro e `:n` /
`:prev` percorrem a lista de argumentos. Um ficheiro que ainda não
existe é um `[New File]`, criado na primeira gravação.

Comandos do modo normal (o núcleo vim implementado):

- Movimentos: `h j k l`, as setas, `w W b B e E`, `0 ^ $`,
  `f F t T` com repetições `;`/`,`, `gg G`, `{ }`, `%`, `H M L` e
  `Enter`. Um prefixo de contagem repete um movimento: `3w`.
- Operadores: `d` (apagar), `c` (mudar), `y` (copiar), aplicados sobre
  qualquer movimento ou objeto de texto (`iw aw i( a( i[ i{ i" i' i<` e
  os seus pares); dobrados (`dd cc yy`) atuam sobre linhas inteiras.
  Abreviaturas: `x X s S D C Y r ~ J`.
- Registos: `"a`–`"z` antes de um operador ou de um put selecionam um
  registo nomeado; as maiúsculas acrescentam. `p`/`P` colam depois/antes
  do cursor.
- Histórico de anulação: `u` anula mudanças inteiras, `Ctrl-R` refaz e
  `.` repete a última mudança (incluindo o texto inserido).
- Procura: `/pattern` para a frente, `?pattern` para trás, `n`/`N`
  repetem, `*` encontra a palavra sob o cursor. Os padrões suportam
  literais, `.`, `*`, `^`, `$`, classes `[...]` e limites de palavra
  `\<` `\>`. As correspondências ficam realçadas até `:noh`.
- Seleção visual: `v` (carateres) e `V` (linhas), estendida por
  qualquer movimento ou objeto de texto, e depois operada com
  `d x c s y J`.
- Deslocamento: `Ctrl-D Ctrl-U` (meia janela), `Ctrl-F Ctrl-B` e
  PageUp/PageDown (janela inteira); `Ctrl-G` mostra o resumo do
  ficheiro.

O núcleo de comandos ex (`:`): `:w [file]`, `:q`, `:wq`, `:x`,
`:e file`, `:enew`, `:r file`, `:n`, `:prev`, `:noh`, `:set number` /
`:set nonumber`, endereços de linha (`:12`, `:$`, `:.+2`),
`:[range]d` e `:[range]s/pattern/replacement/[g]` (com `&` para toda a
correspondência na substituição, `%` para todas as linhas do
intervalo). Um `!` depois de `w`, `q` ou `e` força a passagem pela
postura de só leitura ou por alterações não gravadas.

Tudo o que o vim traz além deste núcleo está faseado para etapas
posteriores; a lista de faseamento vive em `plans/VIM.md` na árvore de
código.

## OPTIONS

- `-R` — só leitura: o buffer edita-se em memória mas `:w` é recusado
  salvo se forçado com `:w!`.
- `+num` — começar na linha `num` do primeiro ficheiro.
- `+` — começar na última linha do primeiro ficheiro.
- `+/pattern` — começar na primeira correspondência de `pattern` no
  primeiro ficheiro.
- `--` — fim das opções; cada argumento seguinte é um nome de
  ficheiro.
- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair.

## EXIT STATUS

- `0` — a sessão terminou com um comando de saída, ou a ajuda curta
  foi mostrada.
- `1` — o terminal falhou; a razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).
- `TERM` — o perfil de terminal que a sessão conduz; valores
  desconhecidos degradam para a base «dumb».

## SEE ALSO

- `man`
- `cat`
