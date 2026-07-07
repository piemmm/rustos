## NAME

users — administrar contas de utilizador e grupos

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

Executa a sessão interativa de administração de contas sobre a
interface protegida `users_admin`. Cada operação é decidida do lado do
núcleo sob a sua identidade atestada pelo núcleo: sem `CAP_USER_ADMIN`
no teto da sua conta, cada operação é recusada no despacho. As
palavras-passe são lidas com o eco do terminal desligado e resumidas
criptograficamente do lado do cliente num registo com sal; o texto
simples nunca atravessa a interface e nunca é ecoado nem registado.

A ferramenta não aceita operandos: as contas administram-se com
comandos escritos dentro da sessão.

- `list` — listar as contas de utilizador.
- `groups` — listar os grupos.
- `create <name> <uid> <gid>` — criar uma conta.
- `passwd <name>` — definir a palavra-passe de uma conta.
- `lock <name>`, `unlock <name>` — desativar ou reativar uma conta.
- `grant <name> <CAP_...>`, `revoke <name> <CAP_...>` — editar as
  concessões de capacidades de uma conta.
- `deluser <name>` — apagar uma conta.
- `addgroup`, `delgroup` — criar ou apagar um grupo.
- `help` — listar os comandos da sessão.
- `exit`, `quit` — terminar a sessão.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste próprio comando e sair.

## EXIT STATUS

- `0` — a sessão terminou limpa, ou a ajuda curta foi mostrada.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `man`
