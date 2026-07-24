## NAME

sysmon — observar ao vivo a memória, as caches e a carga do núcleo

## SYNOPSIS

`sysmon [-d seg.décimos] [-h | -?]`

## DESCRIPTION

`sysmon` é uma vista ao vivo, em ecrã inteiro, do que o núcleo faz com a
memória e a CPU, lida inteiramente através da API de informação do
sistema — não há `/proc` a raspar. Mostra a memória física e a sua
composição, a pilha do núcleo, a banda de pressão de memória e o seu
historial recente, o registo das caches recuperáveis com as **proporções
de acertos** por classe, o nível comprimido `ramzip`, o total de memória
fixada, o uso de armazenamento dos volumes montados, a carga por CPU, a
tabela de interrupções do núcleo e um censo de processos. Mantém-se
utilizável enquanto o sistema está sob carga deliberada e repousa entre
atualizações quando está ocioso (a leitura estaciona; nunca gira em vazio).

Ao arrancar, o monitor fixa a sua própria memória (`mem_pin`, que exige
`CAP_MEM_PIN`) para nunca encalhar nas suas próprias falhas de página sob
a mesma pressão que observa. Uma fixação recusada é comunicada na linha de
título e a sessão continua sem fixação — a fixação é acessória, nunca
fatal.

O ecrã atualiza-se a cada intervalo (3,0 segundos salvo que `-d` o altere).
O monitor não aceita operandos: é conduzido por teclas premidas dentro da
sessão.

- `q` — sair.
- Esquerda / Direita (ou `p`) — mudar o painel de detalhe (Esquerda =
  anterior, Direita / `p` = seguinte): caches, o nível comprimido, o
  armazenamento dos volumes montados (discos), a carga por CPU, as linhas
  de interrupção, os processos.
- `r` — atualizar agora.
- `+` / `-` — alongar / encurtar o intervalo em um segundo, entre 0,1 e 60
  segundos.
- Cima/Baixo, PáginaCima/PáginaBaixo, Início/Fim — deslocar o painel focado.
- `h`, `?` — mostrar ou ocultar o resumo de teclas da sessão (que reproduz
  a legenda das barras abaixo).

### O bloco de resumo

Um bloco de resumo fixo precede o painel de detalhe. Cada linha está
rotulada à esquerda para se ler sem cor; a cor é apenas reforço.

- **Linha de título** — o nome da ferramenta, o tempo de atividade do
  sistema (`up D days, H:MM`), as três médias de carga (1/5/15 minutos) e o
  estado de fixação (`[pinned]`, ou `[unpinned: <reason>]` quando a fixação
  foi recusada).
- **`Mem`** — a barra de memória (ver a legenda das barras), seguida dos
  MiB usados / totais, da percentagem usada, do tamanho da pilha do núcleo
  e — quando não nulos — dos valores do armazém comprimido `ramzip` e da
  memória fixada `pinned`.
- **`Pres`** — a barra de pressão de memória: um indicador de cinco bandas,
  cada banda alcançada preenchida na sua própria cor de gravidade, seguida
  do nome da banda atual, dos valores livre / reserva e do total de
  entradas em banda.
- **`Hist`** — a tira do historial de bandas de pressão: um glifo por
  atualização, o mais antigo à esquerda, cada um colorido pela sua banda —
  `.` normal, `-` ligeira, `=` moderada, `#` grave, `!` crítica — de modo
  que um trecho de pressão se lê como uma sequência colorida.
- **`CPU`** — a barra CPU agregada (ver a legenda das barras), seguida da
  percentagem ocupada de todas as CPU, do número de CPU e dos contadores
  somados de mudanças de contexto e de preempções.
- **`Tasks`** — o censo de processos: totais, em execução, a dormir,
  parados e zombis (com `(own)` acrescentado quando o censo de todos os
  processos foi recusado e só se contam as tarefas próprias).
- **Barra de separadores de painéis** — cada painel de detalhe, o focado
  realçado, com um indicador de deslocamento à direita quando o painel
  focado transborda.

### A legenda das barras

Os indicadores `Mem` e `CPU` são barras entre parênteses retos `[…]`. O
resumo `?` reproduz esta legenda dentro da sessão em curso.

A barra de memória (`Mem`) é uma barra **empilhada** cujas células nomeiam
o que a memória física contém — uma repartição *disjunta* da memória usada
(`used` é `total` menos `free`), de modo que nada é contado duas vezes e a
largura preenchida é exatamente a fração usada:

- `#` — memória residente de utilizador (verde): páginas residentes nos
  espaços de endereçamento de utilizador.
- `K` — a pilha do núcleo (ciano): as pilhas e lajes próprias do núcleo.
- `=` — outra memória em uso (magenta): tudo o que está usado mas não
  atribuído acima (caches de páginas, buffers, molduras do núcleo).
- em branco — memória livre.

O armazém comprimido `ramzip` e a memória anónima `pinned` sobrepõem-se a
esses baldes (as páginas fixadas são residentes de utilizador; o armazém
comprimido é memória do núcleo), pelo que são comunicados como valores ao
lado da barra em vez de como segmentos separados que contariam a dobrar —
contabilidade honesta em vez de uma imagem enganadora.

A barra de pressão (`Pres`) colore cada banda pela sua profundidade:
normal/ligeira verde, moderada amarela, grave/crítica vermelha.

A barra CPU (`CPU`) preenche-se com células ocupadas `#` sobre pista ociosa
em branco, colorida pela quota ocupada (verde abaixo de 60 %, amarelo abaixo
de 85 %, vermelho a 85 % ou mais). TAIRiX contabiliza o tempo de CPU apenas
como ocupado versus ocioso — não há repartição utilizador/sistema/e-s na
API — pelo que a barra mostra uma única categoria honesta de ocupação, com
o detalhe por núcleo no painel `cpu`.

### Os painéis de detalhe

Esquerda / Direita (ou `p`) percorre seis painéis. Cada um tem um cabeçalho
de coluna invertido (vídeo inverso, negrito) para que o título se leia como
uma barra distinta acima do corpo.

### caches — o registo das caches recuperáveis

São as caches que o núcleo pode devolver para aliviar a pressão de memória
**sem perda de dados**: cada entrada é reconstruível a partir da sua fonte
canónica, pelo que o núcleo a descarta em vez de a paginar. O painel é a
resposta direta a «estão as caches a fazer o seu trabalho?»: cada linha é
uma classe de recuperação, agregada sobre todas as caches registadas, e
traz a sua própria **proporção de acertos**.

Colunas:

- `class` — a classe de recuperação (ver a lista de classes abaixo).
- `entries` — entradas vivas atualmente retidas para a classe.
- `cached` — a pegada residente da classe: a carga útil das entradas mais
  os metadados de contabilidade por entrada, juntos.
- `hits` — procuras da classe servidas a partir da cache desde o arranque
  (a cache evitou a fonte canónica).
- `misses` — procuras da classe que caíram na fonte canónica desde o
  arranque.
- `hit%` — a proporção de eficácia da cache, `hits / (hits + misses)` como
  percentagem inteira. Uma proporção alta significa que a cache rentabiliza
  a sua memória; uma baixa, que retém memória sem evitar trabalho. Lê `-`,
  nunca um `0%` fabricado, para uma classe que nada procurou neste arranque
  (um denominador ocioso).
- `ref` — admissões **recusadas** desde o arranque (uma entrada que a cache
  declinou reter: fora do orçamento, não contabilizável, ou sem memória).
- `shr` — passagens de **encolhimento** forçado pela pressão que
  recuperaram entradas da classe desde o arranque.
- `fail` — **falhas** internas atribuídas à classe: um defeito de registo
  detetado que envenenou (desativou fail-closed) uma cache.

As contagens abreviam-se acima de 99 999 como `k`/`M`/`G`/`T` (milhares
decimais, não KiB) para que uma coluna nunca se alargue.

As classes de recuperação, pela ordem em que o núcleo as recupera sob
pressão (a primeira listada é descartada primeiro, pelo que uma cache baixa
na lista sobrevive mais tempo):

- `disposable-ui` — estado de interface descartável (recursos rasterizados,
  atlas de glifos, instantâneos de janela): o mais barato de perder, o
  primeiro a partir.
- `predictive-prefetch` — dados pré-carregados de forma especulativa
  (listagens, miniaturas, índices de conclusão): nunca necessários para a
  correção.
- `background-validation` — produtos de trabalho de validação em tempo
  ocioso (progresso de varredura, impressões digitais candidatas): o
  trabalho especulativo para assim que a pressão começa.
- `semantic-app-cache` — estado verificado de arranque de aplicações
  (manifestos analisados, resumos de validação, resultados de resolução de
  comandos). Recuperá-lo nunca pode tornar uma aplicação não arrancável — o
  portão de carregamento simplesmente reexecuta.
- `runtime-cache` — estado derivado detido pelo runtime (preparação do
  carregador, mapas de recursos): agrupado com a cache semântica.
- `clean-file-data` — *conteúdo* de ficheiro limpo e reconstruível,
  relegível a partir do volume: uma leitura de dispositivo limitada
  reconstrói um bloco. Recuperado antes de qualquer coisa ser comprimida
  em `ramzip`.
- `transform-cache` — formas intermédias dispendiosas de dados autorizados
  (dados de cluster verificados, decifrados, descomprimidos): mais
  dispendiosas de reconstruir que uma leitura limpa, pelo que são
  recuperadas depois dos dados de ficheiro limpos.
- `fs-metadata` — metadados do sistema de ficheiros: registos de estado,
  resultados de procura de nomes, entradas de diretório e registos de
  segurança. Pequenos, quentes e reconstruídos apenas por um percurso da
  árvore em vários passos, pelo que sobrevivem aos dados de ficheiro sob
  pressão.
- `reliability-assist` — estado reconstruível de assistência à recuperação
  (janelas de verificação, resumos de saúde): justificado pela latência de
  recuperação, pelo que é preservado mais tempo.

### ramzip — o nível de memória comprimida

`ramzip` comprime páginas anónimas frias num armazém menor em RAM em vez de
as paginar. As suas secções:

- `tier` — a pegada viva: `entries` retidas, bytes `logical` (não
  comprimidos) representados, bytes `stored` (cifrados) realmente retidos e
  bytes `metadata` de contabilidade; depois `saved` (lógico menos
  armazenado) com a sua percentagem do lógico — a memória que o nível
  recupera.
- `capacity` — os tetos derivados a que o nível se dimensiona: `min`
  (sempre disponível), `soft` (alvo), `hard` (teto) e os bytes `pinned`
  atuais.
- `compress` — o caminho de armazenamento (escrita): `attempts` oferecidos,
  `accepted` e armazenados, e a **taxa de aceitação** (aceites / tentativas)
  — a proporção de acertos própria deste nível para a compressão. Abaixo, a
  discriminação de rejeições: incompressível, política, teto, inelegível,
  reserva, quota de tarefa e recusas por thrash.
- `restore` — o caminho de recuperação (leitura): `faults` de página,
  restauros `warm`, restauros `clustered` e o seu total `restored`; depois
  as `failures` (autenticação / descodificação) e a **taxa de sucesso**
  (restaurados / (restaurados + falhas)). Cada proporção é uma percentagem,
  ou `-` para um denominador ocioso.
- `warm-up` — as `attempts` do restaurador a quente em segundo plano, a sua
  contagem `stopped` e a sua contagem `thrash-detected`.

### disks — armazenamento dos volumes montados

Uma linha ao estilo `df` por volume montado: ponto de montagem, tipo de
sistema de ficheiros, tamanho total, usado, disponível, percentagem de uso
e uma barra de uso ASCII. Um volume cujo controlador não reporta capacidade
mostra `capacity unknown` em vez de um tamanho fabricado; um volume
removido por surpresa ou em conflito de recuperação é desenhado na
representação de aviso e marcado (`[unavailable-dirty]`,
`[unavailable-lost]`, `[recovery-conflict]`). Não há contadores de débito
de e-s por dispositivo na API, pelo que isto é capacidade e uso honestos,
não taxas de transferência fabricadas.

### cpu — carga por CPU

Uma linha por CPU: a sua quota ocupada no intervalo (`busy%`), a
profundidade da sua fila de execução (`queue`) e as suas contagens de
mudanças de contexto (`switches`) e de preempções (`preemptions`) desde o
arranque.

### irqs — linhas de interrupção

Uma linha por linha de interrupção ligada, em ordem crescente de linha: o
id da linha, a tarefa controladora proprietária (`owner`), a `count` de
interrupções desde o arranque e o `state` da linha — `active`, ou
`quarantined` (desenhado na representação de aviso) quando a rede de
segurança do núcleo contra linhas desgovernadas a desativou.

### procs — o censo de processos

Os maiores consumidores por `%cpu` e por memória (`size`), cada um com o
seu pid, o seu comando e — para a tabela de memória — o seu estado. A lista
interativa completa de processos é tarefa do `top`; isto é apenas o resumo
do censo.

### Capacidades

Cada valor viaja pela API de informação do sistema. As consultas de
estatísticas do núcleo (memória, pressão, caches, `ramzip`, carga por CPU)
exigem `CAP_SYSINFO_KERNEL`; o painel das linhas de interrupção exige
`CAP_SYSINFO_HW`; o censo de todos os processos exige `CAP_SYSINFO_GLOBAL`.
Um chamador sem uma vê a recusa desse painel explicitada — nunca um valor
fabricado — enquanto o resto da sessão continua (falhar fechado, degradar
com graça). O armazenamento dos volumes montados não está restringido.

## OPTIONS

- `-d, --delay <seconds>` — o intervalo entre atualizações automáticas, em
  segundos com fração opcional (só o primeiro dígito decimal, os décimos, é
  conservado): `sysmon -d 1.5` atualiza a cada 1,5 segundos. Predefinição
  3,0. O GNU `top` aceita um intervalo zero e atualiza tão depressa quanto
  pode; TAIRiX nunca gira em vazio, pelo que um zero é elevado ao mínimo de
  0,1 s.
- `-h, -?` — mostrar a ajuda breve deste comando e sair. Dentro de uma
  sessão em curso, as mesmas teclas alternam o resumo de teclas.

## EXIT STATUS

- `0` — a sessão terminou com `q`, ou a ajuda breve foi mostrada.
- `1` — o terminal falhou; o motivo é escrito na saída de erro.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda breve (uma etiqueta BCP-47 como
  `pt-PT`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
