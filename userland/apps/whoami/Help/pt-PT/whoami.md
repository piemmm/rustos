## NAME

whoami — imprimir o nome de conta do utilizador atual

## SYNOPSIS

`whoami`

## DESCRIPTION

Imprime o nome de utilizador associado à identidade deste processo,
seguido de uma mudança de linha, e nada mais.

O RustOS não tem `/etc/passwd`: o identificador de utilizador provém
do registo que o núcleo mantém do processo chamador, e o nome de conta
correspondente provém do diretório público de contas da API de
informação do sistema. Se o diretório não contiver nenhum nome para o
identificador, o comando comunica `cannot find name for user ID <uid>`
e falha.

O comando não aceita operandos; um argumento é um erro
`extra operand`.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste comando.
- `--` — terminar a análise de opções; qualquer argumento posterior
  continua a ser um operando a mais (`whoami` não aceita nenhum).

## EXAMPLES

- `whoami` — imprimir o nome da conta que executa o comando.

## EXIT STATUS

- `0` — o nome (ou a ajuda curta pedida) foi escrito.
- `1` — a leitura da identidade, a consulta do diretório ou a saída
  falhou, ou o diretório não contém nenhum nome para o identificador
  de utilizador.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `users`
- `ps`
