## NAME

usermod — modificar uma conta de utilizador

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

Altera os campos de identidade de uma conta, o seu estado de bloqueio ou o
seu tecto de capacidades. Modificar uma conta é uma operação administrativa:
a base recusa quem não possua a capacidade de administração de utilizadores.

Uma edição de identidade substitui todo o conjunto de campos não
relacionados com segurança, pelo que a ferramenta lê primeiro o registo
actual e reenvia inalterado cada campo que ninguém nomeou. Uma conta que a
base não lista é recusada antes de se enviar seja o que for.

Cada opção é a sua própria operação da base, aplicada uma de cada vez e por
inteiro ou nada. Uma linha que peça várias emite várias, numa ordem fixa —
campos, depois capacidades, depois bloqueio — e pára na primeira recusa,
nomeando o passo e avisando que uma alteração anterior pode já estar em
vigor.

`-G` substitui todo o conjunto suplementar em vez de acrescentar: a base
aceita conjuntos inteiros, e acrescentar sobre uma leitura desactualizada
seria pior do que uma substituição explícita. `--grants` é um conceito
próprio da TAIRiX, escrito apenas na forma longa; a base recusa qualquer
capacidade que a conta chamadora não possua.

`--` termina a análise de opções: todos os argumentos seguintes são operandos.

## OPTIONS

- `-c, --comment COMMENT` — o comentário / nome completo da conta.
- `-d, --home HOME` — o directório pessoal.
- `-s, --shell SHELL` — a shell de início de sessão.
- `-g, --gid GID` — o identificador numérico do grupo principal.
- `-G, --groups LIST` — os identificadores numéricos dos grupos
  suplementares separados por vírgulas, substituindo o conjunto actual. Uma
  lista vazia limpa-o.
- `-L, --lock` — impedir o início de sessão da conta.
- `-U, --unlock` — voltar a permiti-lo.
- `--grants LIST` — os nomes de capacidades separados por vírgulas que
  formam todo o tecto. Uma lista vazia limpa-o.
- `-h, -?, --help` — mostrar a ajuda curta do próprio comando.

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — definir o nome completo da conta.
- `usermod -L ada` — bloquear a conta.
- `usermod --grants LIST` — substituir o tecto de capacidades.

## EXIT STATUS

- `0` — todas as alterações pedidas foram feitas.
- `1` — a base recusou ou falhou uma alteração; o passo e a razão são
  escritos no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda curta (uma etiqueta BCP-47 como `pt-PT`).

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
