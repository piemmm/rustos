## NAME

yes — escrever repetidamente uma linha de texto

## SYNOPSIS

`yes [string...]`

## DESCRIPTION

Escreve os seus operandos, unidos por espaços simples — ou `y` quando
não é dado nenhum —, seguidos de uma mudança de linha, vezes sem conta,
até a saída deixar de aceitar bytes (um pipe fechado) ou o processo ser
terminado. O seu papel histórico é fornecer uma resposta afirmativa a um
comando que pergunta; o moderno é ser uma fonte barata de texto
repetido.

A análise de opções para no primeiro operando, pelo que `yes a -x`
escreve `a -x`. Uma opção desconhecida antes dos operandos é um erro;
escreva `yes -- -x` para imprimir uma cadeia que parece uma opção.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando.
- `--` — terminar a análise de opções; cada argumento posterior é um
  operando.

## EXAMPLES

- `yes` — imprimir `y` até ser interrompido.
- `yes hello world` — imprimir `hello world` até ser interrompido.
- `yes -- -x` — imprimir `-x` (depois de `--`, os operandos podem
  parecer opções).

## EXIT STATUS

- `0` — uma ajuda curta pedida foi servida.
- `1` — a saída deixou de aceitar bytes (a única condição de paragem da
  ferramenta).
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `true`
- `man`
