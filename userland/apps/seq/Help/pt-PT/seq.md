## NAME

seq — imprimir uma sequência de números

## SYNOPSIS

`seq [-f format] [-s string] [-w] [first [increment]] last`

## DESCRIPTION

Imprime os números de `first` a `last`, em passos de `increment`, um
por linha por omissão. Um `first` ou `increment` omitido vale 1 —
inclusive quando `last` é menor do que `first`, pelo que `seq 5 1` não
imprime nada. A sequência termina quando somar `increment` passaria
`last`.

Os três operandos são lidos como valores de vírgula flutuante;
`increment` é normalmente positivo quando `first` está abaixo de `last`
e negativo quando está acima, e não pode ser zero. `last` pode ser
`inf` para contar para sempre. A precisão de saída por omissão segue a
grafia dos operandos (`seq 1 0.25 2` imprime duas casas decimais), e as
sequências de inteiros simples são geradas exatamente, por maiores que
sejam os números.

A análise de opções para no primeiro operando, e um número negativo
inicial é um operando, não uma opção: `seq -5 5` conta a partir de -5.

## OPTIONS

- `-f, --format <format>` — imprimir cada número através do `<format>`
  de vírgula flutuante ao estilo printf (uma diretiva `%` do tipo `e`,
  `f`, `g` ou `a`, maiúscula ou minúscula, com as bandeiras, largura e
  precisão habituais). Não pode ser combinado com `-w`.
- `-s, --separator <string>` — separar os números com `<string>` em vez
  de mudança de linha. A saída termina mesmo assim com mudança de
  linha.
- `-w, --equal-width` — preencher cada número com zeros à esquerda até
  uma largura comum. Não pode ser combinado com `-f`.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.
- `--` — terminar a análise de opções; cada argumento posterior é um
  operando.

## EXAMPLES

- `seq 5` — imprimir de 1 a 5.
- `seq 2 5` — imprimir de 2 a 5.
- `seq 1 2 10` — imprimir os ímpares de 1 a 9.
- `seq 5 -1 1` — contar de 5 para 1.
- `seq -w 8 10` — imprimir `08`, `09`, `10`.
- `seq -s , 3` — imprimir `1,2,3`.
- `seq -f %.2f 3` — imprimir `1.00`, `2.00`, `3.00`.

## EXIT STATUS

- `0` — a sequência (ou a ajuda curta pedida) foi escrita.
- `1` — a saída deixou de aceitar bytes.
- `2` — a linha de comandos não foi compreendida (uma opção
  desconhecida, um número inválido, um incremento zero ou um formato
  mau).

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `yes`
- `man`
