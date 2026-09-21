## NAME

userdel — eliminar uma conta de utilizador

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

Remove uma conta da base de dados de utilizadores. Eliminar uma conta é uma operação administrativa: a base recusa quem não possua a capacidade de administração de utilizadores.

A base decide o que pode ser removido. Recusa eliminar a última conta activa que pode administrar utilizadores, para que um sistema nunca fique sem forma de ser administrado.

`--` termina a análise de opções: todos os argumentos seguintes são operandos.

## OPTIONS

- `-h, -?, --help` — mostrar a ajuda curta do próprio comando.

## EXAMPLES

- `userdel ada` — eliminar a conta `ada`.

## EXIT STATUS

- `0` — a conta foi eliminada.
- `1` — a base recusou ou falhou a eliminação (por exemplo uma capacidade em falta, uma conta desconhecida ou o último administrador); a razão é escrita no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda curta (uma etiqueta BCP-47 como `pt-PT`).

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
