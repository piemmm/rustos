## NAME

ls — listar o conteúdo de diretórios

## SYNOPSIS

`ls [-aACdFghlmnopQrRsSx1] [-w cols] [--] [path...]`

## DESCRIPTION

Lista cada operando de caminho: as entradas de um operando de diretório
são lidas e listadas (salvo se `-d` nomear o próprio diretório);
qualquer outro operando é listado como ele próprio. Sem operando,
lista-se o diretório atual (`.`).

As entradas são ordenadas por nome (ou por tamanho, do maior para o
menor, com `-S`; invertidas com `-r`), um nome por linha por omissão.
As entradas cujo nome começa por `.` são ocultadas salvo se `-a` ou
`-A` for dado; quando há entradas ocultadas, emite-se uma nota no
fluxo de informação padrão (fd 3), nunca na própria listagem.

O formato longo (`-l`) apresenta os bits de tipo e permissão, o dono e
o grupo, o tamanho e depois o nome. O dono e o grupo são ids numéricos:
resolver nomes de conta exigiria a base de dados de utilizadores
protegida por capacidade, que uma listagem não deve exigir, pelo que a
saída corresponde ao recurso numérico da ferramenta GNU (`-n` produz o
mesmo). Não há coluna de contagem de ligações nem de datas porque o
contrato do sistema de ficheiros ainda não transporta ligações rígidas
nem datas; as colunas aparecerão quando o fizer.

Quando é dado mais de um operando — e sempre com `-R` — a listagem de
cada diretório é precedida por um cabeçalho `path:` e os blocos são
separados por uma linha em branco.

## OPTIONS

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
