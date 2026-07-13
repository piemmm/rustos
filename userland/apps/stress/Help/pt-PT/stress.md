## NAME

stress — carregar a pedido a CPU, a memória, o disco e as caches

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Lança processos de trabalho que carregam a máquina deliberadamente, no
espírito das ferramentas estabelecidas `stress`/`stress-ng`: ciclos de
CPU (`--cpu`), trabalhadores de memória alocar-e-tocar (`--vm`),
escrita/sincronização de pequenos buffers (`--io`), escritores de disco
sequenciais grandes (`--hdd`) e releitores que agitam as caches
(`--cache`, uma adição do RustOS). Cada trabalhador é o seu próprio
processo paginável; o processo controlador fixa a sua própria memória
(`mem_pin`, requer `CAP_MEM_PIN`) para permanecer reativo sob a pressão
que ele próprio cria, e observa `Ctrl-C`/`Terminate`, de modo que cada
fim da execução — conclusão, tempo limite ou sinal — pára os
trabalhadores, recolhe-os e remove todos os ficheiros de trabalho.

Os alvos de memória e disco são dimensionados a partir da própria
máquina: salvo valores explícitos com `--vm-bytes`/`--hdd-bytes`, os
trabalhadores vm partilham metade da RAM descoberta e os hdd metade do
espaço livre do volume de trabalho. `--overcommit P` reescala esses
alvos descobertos para `P` por cento do recurso; acima de 100 os
trabalhadores empurram para a pressão, e as recusas tipificadas que
isso produz (volume cheio, limite de recursos) são contadas e
comunicadas como resultados esperados — nunca repetidas, nunca uma
falha catastrófica. Carregar a máquina não precisa de privilégio para
além dos próprios limites de recursos do chamador — os limites são a
defesa, e o `stress` respeita-os.

Os trabalhadores que tocam no disco escrevem apenas sob o diretório de
trabalho — o diretório de cache por utilizador da aplicação
(`$HOME/Library/stress`) salvo se `--temp-path` nomear outro — e cada
ficheiro de trabalho é removido no desmantelamento, incluindo nos
caminhos de sinal.

No fim da execução é impresso um resumo (suprimido por `--quiet`), e é
emitido um registo `summary` legível por máquina no fluxo de informação
padrão consultivo (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — lançar `N` trabalhadores
  do tipo indicado, com o significado do GNU `stress`.
- `--cache N` — lançar `N` agitadores de cache (apenas RustOS:
  percursos frios repetidos por diretórios e releituras movem os
  registos de caches recuperáveis do núcleo).
- `--all N` — `N` trabalhadores de cada tipo.
- `--vm-bytes B`, `--hdd-bytes B` — o alvo em bytes de cada
  trabalhador, com os sufixos GNU (`k`, `m`, `g`, `t`; p. ex.
  `256M`). Os valores por omissão são dimensionados a partir da RAM /
  do espaço livre descobertos.
- `--overcommit P` — escalar os alvos vm/hdd descobertos para `P` por
  cento do recurso; pode exceder 100 (as recusas são então resultados
  esperados).
- `--timeout T` — parar após `T` (sufixos `s`/`m`/`h`; p. ex. `5m`).
  Sem valor por omissão: sem ele, a execução continua até um sinal a
  terminar.
- `--temp-path DIR` — o diretório de trabalho dos trabalhadores que
  tocam no disco.
- `--monitor` — executar o `sysmon` em primeiro plano durante a
  execução; esta é comunicada quando o monitor termina. Contradiz
  `--background`.
- `-q, --quiet` — suprimir o resumo e as linhas de progresso no
  stdout (os erros continuam a chegar ao stderr).
- `--background` — imprimir o PID do controlador destacado e devolver
  o prompt (implica `--quiet`). A forma `&` da shell também funciona;
  esta opção é para scripts.
- `-h, -?, --help` — mostrar a ajuda curta deste comando e sair.
- `--version` — imprimir o nome e a versão da ferramenta e sair.

## EXIT STATUS

- `0` — a execução foi concluída (as recusas tipificadas dos
  trabalhadores são resultados esperados e não a fazem falhar).
- `1` — um trabalhador falhou realmente, ou a execução não pôde ser
  preparada.
- `2` — a linha de comandos não foi compreendida.
- `130` / `143` — `Ctrl-C` / `Terminate` terminou a execução, após o
  desmantelamento dos trabalhadores e a remoção dos ficheiros de
  trabalho.

## ENVIRONMENT

- `HOME` — localiza o diretório de trabalho por omissão
  (`$HOME/Library/stress`).
- `LANG` — o locale preferido da ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
