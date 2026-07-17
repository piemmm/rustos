## NAME

dirname — retirar a última componente de nomes

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

Imprime cada grafia de caminho com a sua última componente removida: as
barras finais são retiradas, depois a última componente e as barras
antes dela. A cirurgia é puramente lexical — nenhum caminho é resolvido
nem tocado no disco. Uma grafia sem barra restante tem `.` como pai; um
pai que fica vazio é a raiz.

Uma raiz nunca é desfeita: `dirname /tools` é `/`, e — o equivalente na
floresta de armazenamento do TAIRiX — `dirname Home:/tools` é `Home:/`.
Uma raiz de alias (`Home:/`, `System:/`, …) desempenha exatamente o
papel que `/` desempenha nos sistemas POSIX.

## OPTIONS

- `-z, --zero` — terminar cada resultado com NUL em vez de mudança de
  linha.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `dirname /System/Apps/top.app` — imprimir `/System/Apps`.
- `dirname src/lib.rs` — imprimir `src`.
- `dirname file` — imprimir `.` (sem parte de diretório).
- `dirname Home:/tools` — imprimir `Home:/` (uma raiz nunca é
  desfeita).

## EXIT STATUS

- `0` — os resultados (ou a ajuda curta) foram escritos.
- `1` — a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `basename`
- `man`
