## NAME

sysinfo — consultar informação do sistema

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Emite uma consulta tipada à API de Informação do Sistema e apresenta a
resposta. O TAIRiX não tem `/proc` nem `/sys`: este comando é a face de
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
- `pressure` — o indicador de pressão de memória em direto: banda,
  limiares e contadores de transição (necessita `CAP_SYSINFO_KERNEL`).
- `reclaim` — o registo das caches recuperáveis, uma linha por classe
  (necessita `CAP_SYSINFO_KERNEL`).
- `ramzip` — os contadores do nível de memória comprimida (necessita
  `CAP_SYSINFO_KERNEL`).
- `cpu` — profundidade da fila, mudanças de contexto e preempções por
  CPU (necessita `CAP_SYSINFO_KERNEL`).
- `irq`, `irqs` — a tabela de IRQ do núcleo: uma linha por cada linha de
  interrupção associada — o seu id, a tarefa do controlador proprietária,
  o número de interrupções desde o arranque e se a linha está em
  quarentena (necessita `CAP_SYSINFO_HW`).
- `cpuinfo` — o relatório do processador por CPU (um superconjunto de
  `/proc/cpuinfo`): modelo/fabricante, classe de desempenho, sinalizadores
  de extensões ISA, o registo de identidade em bruto, a frequência de
  relógio do núcleo medida em direto (em MHz — ou um honesto «unknown»
  onde não existe contador de relógio do núcleo) e a frequência fixa de
  referência ou base de tempo. Factos públicos do hardware, não requer
  nenhuma capacidade.
- `storage`, `io` — a saúde de E/S de armazenamento por volume: uma linha
  por cada volume suportado por blocos e ciente de falhas — um prefixo do
  seu identificador durável, o ponto final do serviço de blocos que o
  serve, a sua disponibilidade actual
  (available/degraded/recovering/lost) e os contadores cumulativos de
  resultados (conclusões, reinícios, expirações, erros de suporte,
  reemissões) em que um disco a falhar ou instável se torna visível
  (necessita `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — os conjuntos RAID compostos e os dispositivos que o
  compositor de conjuntos detém: uma linha por conjunto — um prefixo da
  sua identidade, o seu nível, a sua saúde
  (optimal/degraded/recovering/failed), o número de membros sincronizados
  e definidos, a sua unidade de faixa, o seu número de blocos e qualquer
  reconstrução ou verificação em curso — depois uma linha por dispositivo
  — o seu nó da árvore de hardware, o conjunto a que pertence (um travessão
  para um candidato não afiliado), a sua ranhura, o seu papel
  (candidate/held/in-sync/resyncing/faulted), o seu tamanho e a geração de
  metadados que carrega (necessita `CAP_SYSINFO_HW`).
- `show <resource-ref>` — lê uma referência de recurso
  `info:`/`state:`/`stats:` e imprime o seu valor. Esses espaços de nomes
  servem valores tipados através desta API, nunca fluxos de bytes: o `cat`
  não os consegue abrir. Uma recusa nomeia a capacidade necessária.
- `describe <resource-ref>` — imprime o envelope da resposta em vez do
  valor: o produtor, a autorização com que foi servida e os metadados da
  carga — para uma métrica o género, a unidade, o comportamento de
  reposição e a janela de amostragem; para um facto o tipo e a
  sensibilidade.
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
