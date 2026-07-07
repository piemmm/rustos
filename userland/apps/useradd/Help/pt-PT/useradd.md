## NAME

useradd — criar uma conta de utilizador

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

Acrescenta uma única conta à base de dados de utilizadores. O nome de
início de sessão tem de corresponder a `[a-z_][a-z0-9_-]*`; o grupo
primário (`-g`) é obrigatório e cada referência a grupo ou utilizador é
um id decimal. Criar uma conta é uma operação administrativa: a base de
dados recusa um chamador sem a capacidade de administração de
utilizadores.

A conta criada **não tem palavra-passe utilizável**: nenhuma
palavra-passe lhe corresponde até um administrador definir uma (e
nenhuma pode ser adivinhada), exatamente como a ferramenta GNU cria uma
conta desativada. Defina depois uma palavra-passe com o comando
`passwd` da ferramenta `users`.

Quando `-u` é omitido, o id de utilizador é atribuído automaticamente,
um acima do id existente mais alto. Quando `-d` é omitido, o diretório
pessoal é a disposição padrão `/Users/NAME`. A conta começa com a shell
por omissão do sistema e o teto ordinário de capacidades de sessão; um
administrador alarga-o depois com o comando `grant` da ferramenta
`users`.

`--` termina a análise de opções: cada argumento posterior é um
operando.

## OPTIONS

- `-u, --uid UID` — id numérico do utilizador; atribuído
  automaticamente quando omitido (um acima do id existente mais alto).
- `-g, --gid GID` — id numérico do grupo primário. Obrigatório: não há
  política de grupo por omissão a adivinhar.
- `-G, --groups LIST` — ids numéricos de grupos suplementares,
  separados por vírgulas.
- `-c, --comment TEXT` — comentário da conta / nome completo de
  exibição.
- `-d, --home PATH` — diretório pessoal; `/Users/NAME` quando omitido.
- `-h, -?, --help` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `useradd -g 100 alice` — criar `alice` no grupo primário `100` com um
  id atribuído automaticamente.
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — todos os
  campos explicitados.

## EXIT STATUS

- `0` — a conta foi criada.
- `1` — a base de dados recusou ou falhou a criação (por exemplo uma
  capacidade em falta, um id duplicado ou um grupo desconhecido); a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `groupadd`
- `users`
