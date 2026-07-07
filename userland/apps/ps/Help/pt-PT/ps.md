## NAME

ps — listar processos

## SYNOPSIS

`ps [-e | -A | --all] [-h | -?]`

## DESCRIPTION

Lista processos através da API de Informação do Sistema. Por omissão só
são listados os processos do próprio chamador; o serviço aplica cada
âmbito de consulta contra a identidade do chamador atestada pelo
núcleo, e não há caminho que contorne essa verificação.

Cada processo é impresso como uma fila sob um cabeçalho de colunas: o
id do processo (`PID`), o id do processo pai (`PPID`), os ids do
utilizador e do grupo donos (`UID`, `GID`), o estado de escalonamento
(`S`), o CPU em que o processo correu por último (`CPU`) e o nome do
comando (`NAME`).

O `ps` não aceita operandos.

## OPTIONS

- `-e, -A, --all` — listar todos os processos do sistema em vez de
  apenas os do chamador; o serviço só concede esta vista a um chamador
  que detenha `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `ps` — listar os seus próprios processos.
- `ps -e` — listar todos os processos do sistema.

## EXIT STATUS

- `0` — a listagem foi escrita.
- `1` — o serviço recusou ou falhou, ou a listagem não pôde ser
  entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `man`
- `top`
- `sysinfo`
