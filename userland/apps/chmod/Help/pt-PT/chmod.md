## NAME

chmod — alterar os bits de modo de um ficheiro

## SYNOPSIS

`chmod [-cfRv] [--] MODE file...`

## DESCRIPTION

Altera os bits de permissão de cada operando de ficheiro para `MODE`,
por ordem. `MODE` é um valor octal absoluto (`644`, `0755`, …) que
substitui por completo os bits de permissão, ou uma lista de cláusulas
simbólicas separadas por vírgulas `[ugoa]*[-+=][rwxXst]*` (`g+w`,
`o-rx`, `a=rx`, `u+s`) que transformam os bits atuais do ficheiro. O
`X` simbólico concede execução apenas a um diretório ou a um ficheiro
que já tenha um bit de execução.

Só o proprietário de um ficheiro pode alterar o seu modo; o núcleo
recusa qualquer outro, e possuir uma capability não concede qualquer
privilégio. Com `-R` um operando de diretório é alterado e depois o
seu conteúdo é alterado recursivamente. A primeira falha para a
execução antes de qualquer operando posterior. `--` termina a análise
de opções: cada argumento posterior é um operando. Para um modo que
comece por `-`, escreva-o sem o traço (`a-w`) ou termine primeiro as
opções (`chmod -- -w file`).

## OPTIONS

- `-R, --recursive` — alterar ficheiros e diretórios
  recursivamente.
- `-c, --changes` — reportar apenas os ficheiros cujo modo mudou
  realmente.
- `-v, --verbose` — reportar cada ficheiro processado.
- `-f, --silent, --quiet` — suprimir a maioria das mensagens de
  erro; a execução continua a falhar e o estado de saída reporta-o.
- `-h, -?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `chmod 644 notes.txt` — leitura/escrita para o proprietário,
  apenas leitura para os restantes.
- `chmod g+w shared.txt` — acrescentar escrita de grupo aos bits
  atuais.
- `chmod -R a=rx Docs` — tornar a árvore `Docs` legível e
  atravessável por todos.

## EXIT STATUS

- `0` — todas as alterações de modo tiveram êxito.
- `1` — uma falha do sistema de ficheiros ou da saída; a razão é
  impressa na saída de erro (suprimida sob `-f`).
- `2` — a linha de comandos não foi compreendida, ou o operando de
  modo não era octal nem simbólico.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `ls`
- `mkdir`
- `rm`
