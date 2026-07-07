## NAME

groupadd — criar um grupo

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

Acrescenta um único grupo ao registo de grupos. O nome do grupo tem de
corresponder a `[a-z_][a-z0-9_-]*` e o id é um valor decimal. Criar um
grupo é uma operação administrativa: o registo recusa um chamador sem a
capacidade de administração de utilizadores.

Quando `-g` é omitido, o id do grupo é atribuído automaticamente, um
acima do id existente mais alto. Um id pedido que já esteja ocupado é
recusado; o registo é a autoridade sobre colisões.

`--` termina a análise de opções: cada argumento posterior é um
operando.

## OPTIONS

- `-g, --gid GID` — id numérico do grupo; atribuído automaticamente
  quando omitido (um acima do id existente mais alto).
- `-h, -?, --help` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `groupadd staff` — criar `staff` com um id atribuído automaticamente.
- `groupadd -g 100 staff` — criar `staff` com o id `100`.

## EXIT STATUS

- `0` — o grupo foi criado.
- `1` — o registo recusou ou falhou a criação (por exemplo uma
  capacidade em falta ou um id duplicado); a razão é impressa no erro
  padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `useradd`
- `users`
