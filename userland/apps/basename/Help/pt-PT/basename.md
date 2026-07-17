## NAME

basename — retirar o diretório e o sufixo de nomes

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

Imprime a componente final de cada grafia de caminho: as barras finais
são removidas e depois tudo até à última barra restante, inclusive. A
cirurgia é puramente lexical — nenhum caminho é resolvido nem tocado no
disco. Com um `suffix` (o segundo operando, ou `-s`), um `suffix` final
também é removido, salvo se for todo o nome restante.

Uma raiz nunca é desfeita: `basename /` é `/`, e — o equivalente na
floresta de armazenamento do TAIRiX — `basename Home:/` é `Home:/`. Uma
raiz de alias (`Home:/`, `System:/`, …) desempenha exatamente o papel
que `/` desempenha nos sistemas POSIX.

Sem `-a` nem `-s`, aceitam-se no máximo dois operandos: o nome e um
sufixo opcional. Com `-a` (ou `-s`, que o implica), todos os operandos
são nomes.

## OPTIONS

- `-a, --multiple` — tratar todos os operandos como nomes.
- `-s, --suffix <suffix>` — remover um `suffix` final de cada nome;
  implica `-a`. Também se escreve `--suffix=<suffix>` ou agrupado
  (`-s.rs`).
- `-z, --zero` — terminar cada resultado com NUL em vez de mudança de
  linha.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `basename /System/Apps/top.app` — imprimir `top.app`.
- `basename src/lib.rs .rs` — imprimir `lib`.
- `basename -s .rs -a a.rs b.rs` — imprimir `a` e `b`.
- `basename Home:/` — imprimir `Home:/` (uma raiz nunca é desfeita).

## EXIT STATUS

- `0` — os resultados (ou a ajuda curta) foram escritos.
- `1` — a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `dirname`
- `man`
