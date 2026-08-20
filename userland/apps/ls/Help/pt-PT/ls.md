## NAME

ls — listar o conteúdo de diretórios

## SYNOPSIS

`ls [-aABbCcdFfGghikIlmNnopQqrRsSTtUuvXx1] [-w cols] [-I PATTERN]`
`[--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD]`
`[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
`[--quoting-style=STYLE] [--full-time] [--author] [--file-type]`
`[--group-directories-first] [--zero] [--color[=WHEN]] [--] [path...]`

## DESCRIPTION

Lista cada operando de caminho: as entradas de um operando de diretório
são lidas e listadas (salvo se `-d` nomear o próprio diretório);
qualquer outro operando é listado como ele próprio. Sem operando,
lista-se o diretório atual (`.`).

As entradas são ordenadas por nome (ou por tamanho, do maior para o
menor, com `-S`; por data, a mais recente primeiro, com `-t`;
invertidas com `-r`), um nome por linha por omissão.
As entradas cujo nome começa por `.` são ocultadas salvo se `-a` ou
`-A` for dado; quando há entradas ocultadas, emite-se uma nota no
fluxo de informação padrão (fd 3), nunca na própria listagem.

O formato longo (`-l`) apresenta os bits de tipo e permissão, o dono e
o grupo, o tamanho e depois o nome. O dono e o grupo são ids numéricos:
resolver nomes de conta exigiria a base de dados de utilizadores
protegida por capacidade, que uma listagem não deve exigir, pelo que a
saída corresponde ao recurso numérico da ferramenta GNU (`-n` produz o
mesmo). A coluna de data mostra a hora de modificação por omissão;
`-c`, `-u` e `--time` escolhem qual das quatro datas é mostrada (e por
qual se ordena), e `--time-style` — ou `--full-time` — define o seu
formato. Ainda não há coluna de contagem de ligações porque o contrato
do sistema de ficheiros ainda não transporta ligações rígidas;
aparecerá quando o fizer.

Quando é dado mais de um operando — e sempre com `-R` — a listagem de
cada diretório é precedida por um cabeçalho `path:` e os blocos são
separados por uma linha em branco.

Uma ligação simbólica aparece com a letra de tipo `l` e, no formato longo,
como `nome -> alvo` — o alvo exactamente como está guardado, não
resolvido, que é o que a ligação contém. Uma ligação pendente lista-se
portanto normalmente; só uma postura que a resolva (`-L`, ou `-H` para um
operando) comunica um alvo inalcançável.

## OPTIONS

- `-t` — ordenar pela data mostrada, a mais recente primeiro.
- `-c` — usar a hora de alteração de metadados (ctime): com `-l`
  mostrá-la e com `-t` ordenar por ela; sem `-l`, ordenar por ela.
- `-u` — como `-c`, mas a hora de acesso (atime).
- `-i, --inode` — imprimir o número de nó de cada entrada.
- `-B, --ignore-backups` — não listar as entradas cujo nome termina em
  `~`, em todos os modos (as cópias são ocultadas mesmo com `-a`).
- `-I, --ignore=PATTERN` — não listar as entradas que correspondam ao
  glob `PATTERN` (repetível); aplica-se em todos os modos.
- `--hide=PATTERN` — como `--ignore`, mas sem efeito quando `-a` ou `-A`
  é indicado.
- `--time=WORD` — que data mostrar e por qual ordenar: `atime`
  (`access`, `use`), `ctime` (`status`), `mtime` (`modification`) ou
  `birth` (`creation`).
- `--time-style=STYLE` — formato da data: `locale` (predefinição),
  `long-iso`, `full-iso` ou `iso`. Um `+FORMAT` próprio não é
  suportado.
- `--full-time` — como `-l --time-style=full-iso`.
- `-a, --all` — não ocultar as entradas cujo nome começa por `.`.
- `-A, --almost-all` — como `-a`, mas nunca listar `.` nem `..`.
- `-d, --directory` — listar os próprios operandos de diretório, não o
  seu conteúdo.
- `-F, --classify` — acrescentar `/` aos diretórios e `*` aos
  executáveis.
- `-g` — formato longo sem a coluna do dono; implica `-l`.
- `-h, --human-readable` — com `-l`, imprimir tamanhos como `1.1K`,
  `23M` (potências de 1024).
- `-l` — formato longo: bits de permissão, dono, grupo, tamanho e
  depois o nome.
- `-m` — nomes separados por vírgulas, ajustados à largura.
- `-n, --numeric-uid-gid` — formato longo com dono e grupo numéricos;
  implica `-l`. Aqui o dono e o grupo são sempre numéricos (ver
  acima), pelo que corresponde a `-l`.
- `-o` — formato longo sem a coluna do grupo; implica `-l`.
- `-p` — acrescentar `/` aos diretórios.
- `-N, --literal` — imprimir os nomes tal como são, sem aspas
  (`--quoting-style=literal`).
- `-Q, --quote-name` — aspas ao estilo C: pôr cada nome entre aspas
  duplas, escapando aspas, barras invertidas e carateres de controlo
  (`--quoting-style=c`).
- `-b, --escape` — como `-Q` mas sem as aspas envolventes e com os
  espaços escapados (`--quoting-style=escape`).
- `--quoting-style=WORD` — como os nomes são citados: `literal` (`-N`),
  `shell`, `shell-always`, `shell-escape`, `shell-escape-always`,
  `c` (`-Q`) ou `escape` (`-b`). A predefinição é `shell-escape` num
  terminal e `literal` caso contrário; os estilos `locale` e `clocale`
  não são suportados.
- `-q, --hide-control-chars` — mostrar os carateres não gráficos como
  `?` (a predefinição num terminal); afeta apenas os estilos sem
  escape.
- `--show-control-chars` — imprimir os carateres não gráficos tal como
  são (a predefinição quando a saída não é um terminal).
- `-r, --reverse` — inverter a ordem de ordenação.
- `-R, --recursive` — listar os subdiretórios recursivamente.
- `-L, --dereference` — mostrar a informação do ficheiro que cada ligação
  simbólica nomeia, em vez da ligação, onde quer que apareça uma. Uma
  ligação cujo alvo não se consegue alcançar é comunicada na saída de erro e
  a listagem continua, com estado de saída diferente de zero.
- `-H, --dereference-command-line` — desreferenciar apenas as ligações
  simbólicas nomeadas na linha de comandos; as ligações dentro de uma
  listagem mantêm-se ligações. Ganha a última de `-L` e `-H`.
- `--dereference-command-line-symlink-to-dir` — o comportamento por omissão
  quando nenhuma opção de formato impõe outro: uma ligação da linha de
  comandos *para um directório* é desreferenciada, pelo que `ls linkdir`
  lista o directório, enquanto qualquer outra ligação se mostra como
  ligação. `-l`, `-d` e `-F` mostram em vez disso cada ligação.
- `-s, --size` — imprimir o tamanho alocado de cada entrada em blocos
  de 1024 bytes (escalado por `-h`), com uma linha `total` por listagem
  de diretório.
- `-C` — listar em colunas, preenchidas de cima para baixo
  (predefinição num terminal).
- `-S` — ordenar por tamanho, do maior para o menor.
- `-U` — não ordenar; listar as entradas na ordem do diretório.
- `-X` — ordenar por extensão do nome (o texto a partir do último
  `.`), empates por nome.
- `-v` — ordenação «versão» natural, de modo que `f2` precede `f10`;
  empates por nome.
- `-f` — não ordenar e mostrar todas as entradas: ativa `-a` e `-U` e
  desativa `-l` e `-s`. Aplicado na sua posição, pelo que um
  `-l`/`-s`/indicador de ordenação posterior o substitui.
- `--sort=WORD` — escolher a chave de ordenação por nome: `none`
  (`-U`), `size` (`-S`), `time` (`-t`), `version` (`-v`), `extension`
  (`-X`) ou `name`.
- `--group-directories-first` — listar os diretórios antes das outras
  entradas; os diretórios primeiro mesmo com `-r`.
- `-w, --width <cols>` — definir a largura de saída em colunas;
  `0` significa ilimitada.
- `-x` — listar em colunas, preenchidas da esquerda para a direita.
- `-1` — um nome por linha (a omissão).
- `-?` — mostrar a ajuda curta deste próprio comando (`--help` é a
  forma longa).

- `--file-type` — acrescentar `/` aos diretórios, mas nunca `*` aos
  executáveis (`--indicator-style=file-type`).
- `--indicator-style=WORD` — escolher o sufixo indicador por nome:
  `none`, `slash` (`-p`), `file-type` (`--file-type`) ou `classify`
  (`-F`).
- `-G, --no-group` — omitir a coluna de grupo do formato longo; ao
  contrário de `-o`, não seleciona o formato longo por si só.
- `--author` — com `-l`, mostrar a coluna de autor (o utilizador
  proprietário) depois do proprietário e antes do grupo.
- `--si` — como `-h` mas em potências de 1000 (`1.1k`, `23M`).
- `-k, --kibibytes` — usar blocos de 1024 bytes para as células `-s`
  e a linha `total` (já é o valor predefinido; uma opção de tamanho
  tem prioridade).
- `--block-size=SIZE` — escalar os tamanhos de ficheiro e os blocos
  `-s` por SIZE: um inteiro (bytes), ou uma unidade `K`/`M`/`G`/`T`/`P`/
  `E` (1024), uma unidade `KiB` (1024) ou uma unidade `KB` (1000),
  opcionalmente com um coeficiente inteiro.
- `--format=WORD` — escolher a disposição por nome: `long` (`-l`) ou
  `verbose`, `single-column` (`-1`), `vertical` (`-C`), `across` ou
  `horizontal` (`-x`), ou `commas` (`-m`).
- `-T, --tabsize <cols>` — definir o passo de tabulação da grelha de
  colunas (predefinição 8); `0` preenche apenas com espaços.
- `--zero` — terminar cada linha com NUL em vez de nova linha;
  seleciona também a coluna única, a citação literal e os caracteres
  de controlo visíveis.

- `--color[=WHEN]` — colorir os nomes por tipo (diretórios,
  executáveis, ficheiros simples). `WHEN` é `auto` (a predefinição:
  colorir apenas quando a saída é um terminal atestado), `always`
  (colorir mesmo quando não é, p. ex. uma consola série) ou `never`;
  `--color` sem `WHEN` significa `always`. A saída canalizada ou
  redirecionada nunca é colorida.

## EXAMPLES

- `ls` — listar o diretório atual.
- `ls -al /System` — listagem em formato longo de `/System`, incluindo
  as entradas ocultas.
- `ls -lhS` — formato longo, tamanhos legíveis, o maior primeiro.
- `ls -R Documents` — percorrer `Documents` recursivamente, um
  cabeçalho por diretório.
- `ls -F` — marcar os diretórios com `/` e os executáveis com `*`.
- `ls -d Documents` — listar a própria entrada `Documents`, não o seu
  conteúdo.

## EXIT STATUS

- `0` — todos os operandos foram listados.
- `1` — um operando não pôde ser inspecionado, um diretório não pôde
  ser lido, ou a saída não pôde ser entregue.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

- `TERM` — o tipo de terminal, que decide a profundidade de cor da
  saída `--color`. Um `TERM` não definido ou sem cor produz texto
  simples com `auto`.

## SEE ALSO

- `cat`
- `man`
