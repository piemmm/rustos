## NAME

printf — formatar e imprimir dados

## SYNOPSIS

`printf format [argument...]`

## DESCRIPTION

Imprime os `argument`(s) sob o controlo de `format`, como a função C
`printf`. O formato contém três tipos de elementos: caracteres comuns,
copiados para a saída padrão; sequências de escape com barra invertida;
e diretivas de conversão `%`, cada uma convertendo o argumento seguinte.

Os escapes são `\a` (alerta), `\b` (retrocesso), `\c` (terminar toda a
saída de imediato), `\e` (escape), `\f` (avanço de página), `\n` (nova
linha), `\r` (retorno do carro), `\t` (tabulação), `\v` (tabulação
vertical), `\\`, `\"`, `\NNN` (um a três dígitos octais), `\xHH` (um ou
dois dígitos hexadecimais) e `\uHHHH` / `\UHHHHHHHH` (pontos de código
Unicode, quatro ou oito dígitos hexadecimais).

As conversões são `%d`/`%i` (decimal com sinal), `%u` (decimal sem
sinal), `%o`/`%x`/`%X` (octal e hexadecimal), `%e`/`%E`/`%f`/`%F`/
`%g`/`%G`/`%a`/`%A` (vírgula flutuante), `%c` (o primeiro caráter do
argumento), `%s` (cadeia), `%b` (cadeia cujos próprios escapes são
interpretados, o octal escreve-se `\0NNN`), `%q` (cadeia protegida para
reutilização como entrada de shell) e `%%` (um `%` literal). Uma
diretiva aceita as flags C `-`, `+`, espaço, `#`, `0` e `'`, uma largura
de campo e uma precisão; a largura e a precisão podem ser `*`, lendo o
seu valor do argumento seguinte. `%b` e `%q` não aceitam flags, largura
nem precisão.

O formato é reutilizado até todos os argumentos serem consumidos; uma
conversão sem argumento restante imprime zero ou a cadeia vazia. Um
argumento numérico é lido como um número C (hexadecimal `0x`, octal com
`0` inicial, vírgula flutuante, `inf`, `nan`); um `'` ou `"` inicial
converte o ponto de código do caráter seguinte. Um argumento que não é
um número, o é apenas parcialmente ou está fora do intervalo é
diagnosticado na saída de erro e convertido até onde for possível — a
execução continua e termina com o estado `1`. Uma conversão
desconhecida, uma flag numa conversão que não a aceita ou um escape
malformado termina a execução com um diagnóstico.

Duas divergências deliberadas do `printf` GNU: a vírgula flutuante é
calculada em dupla precisão IEEE 754 (o GNU usa `long double`), pelo que
um valor além do intervalo do double imprime `inf`; e um *primeiro*
argumento `-h` ou `-?` mostra esta ajuda curta — tal formato escreve-se
`printf -- -h...`.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste comando (apenas como primeiro
  argumento).
- `--` — terminar a análise de opções; o argumento seguinte é o formato.

## EXAMPLES

- `printf '%s\n' hello` — imprimir `hello` e uma nova linha.
- `printf '%d\n' 0x10` — imprimir `16`.
- `printf '%5.2f|\n' 3.14159` — imprimir ` 3.14|`.
- `printf '%s=%q\n' greeting 'hi there'` — imprimir
  `greeting='hi there'`.
- `printf '%b' 'one\ntwo\n'` — imprimir duas linhas a partir de um só
  argumento.
- `printf '%s-' a b c` — reutilizar o formato: `a-b-c-`.

## EXIT STATUS

- `0` — tudo (ou a ajuda curta pedida) foi escrito.
- `1` — foi diagnosticado um problema de conversão, o formato faltava
  ou era inválido, um escape estava malformado, ou a saída deixou de
  aceitar bytes.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `seq`
- `man`
