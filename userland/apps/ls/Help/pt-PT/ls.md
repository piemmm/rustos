## NAME

ls — listar o conteúdo de diretórios

## SYNOPSIS

`ls [-aACcdFfghilmnopQrRsStUuvXx1] [-w cols] [--time=WORD]`
`[--time-style=STYLE] [--sort=WORD] [--full-time]`
`[--group-directories-first] [--] [path...]`

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

## OPTIONS

- `-t` — ordenar pela data mostrada, a mais recente primeiro.
- `-c` — usar a hora de alteração de metadados (ctime): com `-l`
  mostrá-la e com `-t` ordenar por ela; sem `-l`, ordenar por ela.
- `-u` — como `-c`, mas a hora de acesso (atime).
- `-i, --inode` — imprimir o número de nó de cada entrada.
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
- `-Q, --quote-name` — pôr cada nome entre aspas duplas, escapando
  aspas, barras invertidas e carateres de controlo.
- `-r, --reverse` — inverter a ordem de ordenação.
- `-R, --recursive` — listar os subdiretórios recursivamente.
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

## SEE ALSO

- `cat`
- `man`
