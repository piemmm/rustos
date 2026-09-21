## NAME

groupdel — eliminar um grupo

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

Remove um grupo do registo de grupos. Eliminar um grupo é uma operação administrativa: o registo recusa quem não possua a capacidade de administração de utilizadores.

O registo decide o que pode ser removido. Recusa eliminar um grupo que uma conta ainda referencia, para que nenhuma conta nomeie um grupo inexistente.

`--` termina a análise de opções: todos os argumentos seguintes são operandos.

## OPTIONS

- `-h, -?, --help` — mostrar a ajuda curta do próprio comando.

## EXAMPLES

- `groupdel staff` — eliminar o grupo `staff`.

## EXIT STATUS

- `0` — o grupo foi eliminado.
- `1` — o registo recusou ou falhou a eliminação (por exemplo uma capacidade em falta, um grupo desconhecido ou um grupo ainda referenciado); a razão é escrita no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda curta (uma etiqueta BCP-47 como `pt-PT`).

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
