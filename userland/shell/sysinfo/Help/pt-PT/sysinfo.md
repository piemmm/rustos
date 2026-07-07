## NAME

sysinfo — consultar informação do sistema

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Emite uma consulta tipada à API de Informação do Sistema e apresenta a
resposta. O RustOS não tem `/proc` nem `/sys`: este comando é a face de
terminal da mesma API versionada e verificada por capacidades que todos
os programas usam, e nenhum caminho contorna a verificação de
capacidade.

As consultas:

- `processes`, `ps` — listar processos, uma fila por processo.
- `memory`, `mem` — estatísticas de memória do núcleo (precisa de
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — a árvore de hardware detetada (precisa de
  `CAP_SYSINFO_HW`).
- `identity`, `id` — identidade da máquina e versão do SO.
- `uptime` — tempo desde o arranque e a hora de relógio do arranque.
- `limits`, `rlimits` — os seus limites de recursos efetivos e o uso ao
  vivo.
- `seats` — o inventário de assentos: o dono de cada ecrã e a sua
  consola em primeiro plano (necessita de `CAP_SYSINFO_HW`).
- `help` — a ajuda curta deste próprio comando.

Sem consulta, mostra-se a ajuda curta.

## OPTIONS

- `--all, -a` — com `processes`: listar todos os processos do sistema
  em vez de apenas os seus; o serviço só concede esta vista a um
  chamador que detenha `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — mostrar a ajuda curta deste próprio comando.

## EXAMPLES

- `sysinfo identity` — imprimir a identidade da máquina e a versão do
  SO.
- `sysinfo ps --all` — listar todos os processos do sistema.

## EXIT STATUS

- `0` — a consulta foi respondida e apresentada.
- `1` — o serviço recusou ou falhou, ou o resultado não pôde ser
  entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `man`
- `ps`
- `top`
