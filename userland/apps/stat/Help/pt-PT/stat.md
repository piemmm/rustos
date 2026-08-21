## NAME

stat — relatar o estado de um ficheiro ou de um sistema de ficheiros

## SYNOPSIS

`stat [-Lft] [-c FORMATO | --printf=FORMATO] [--] ficheiro...`

## DESCRIPTION

Relata os campos de um estado lido por operando, na ordem da linha de
comandos.

**Sem `-L` uma ligação simbólica é descrita como ela própria** — é para
isso que esta ferramenta serve ao lado do `ls`. `%N` mostra a ligação e o
alvo que guarda, `%F` diz `symbolic link`, e os tamanhos e as marcas
temporais são os da própria ligação. `-L` resolve a última ligação e
descreve aquilo que nomeia.

`-f` passa ao sistema de ficheiros que contém o operando: as contagens de
blocos e inodes do volume, o seu tamanho de bloco e o tipo que a sua
montagem registou. As duas leituras têm vocabulários de campos
**diferentes**, pelo que um formato é verificado contra o que `-f`
selecciona.

`-c`/`--format` imprime uma cadeia de formato por operando, seguida de
uma nova linha; `--printf` interpreta as sequências de escape e não
acrescenta nenhuma. É a única diferença. Uma directiva aceita os
sinalizadores e a largura do printf (`%-10s`, `%06i`, `%.3n`), para que
um relatório fique em colunas. `-t` é a forma breve de uma linha, em
qualquer das leituras.

Um operando que não possa ser lido é relatado no erro padrão, os
operandos restantes continuam a ser descritos e o comando termina com
estado diferente de zero. Um campo que este sistema não possa fornecer —
um instantâneo das montagens que não pode ler, um uid sem nome no
directório de utilizadores — aparece como `?` ou como `UNKNOWN`, nunca
como um substituto plausível.

É exigido pelo menos um operando. `--` termina a análise de opções.

Quatro campos nomeiam um conceito que o TAIRiX não tem e são
**recusados** pelo nome quando um formato usa um deles, em vez de serem
respondidos com um valor inventado: `%G`, porque a API de informação do
sistema publica um directório de utilizadores e nenhum equivalente para
grupos, pelo que `%g` (o identificador numérico) é o campo honesto; `%t`
e `%T` do vocabulário de ficheiro, porque não há ficheiros especiais de
dispositivo com tipo maior ou menor; e `%t` do vocabulário de sistema de
ficheiros, porque um volume não tem número mágico de tipo — `%T` nomeia o
tipo que a sua montagem registou. A recusa ocorre ao analisar o formato,
antes de qualquer caminho ser tocado.

Dois campos relatam um conceito do TAIRiX em vez de um do Linux. Um
volume é identificado por um id de 16 bytes e não por um número de
dispositivo, pelo que `%d` é esse id em decimal e `%D` em hexadecimal;
comparar o `%d` de dois ficheiros continua a responder exactamente a
«estão no mesmo volume?».

## OPTIONS

- `-L, --dereference` — descrever aquilo que uma ligação simbólica
  nomeia, em vez da própria ligação.
- `-f, --file-system` — descrever o sistema de ficheiros que contém cada
  operando em vez do operando.
- `-c, --format=FORMAT` — imprimir `FORMATO` por operando, seguido de
  uma nova linha.
- `--printf=FORMAT` — como `-c`, mas interpretando as sequências de
  escape e sem nova linha final.
- `-t, --terse` — imprimir os campos numa só linha separada por espaços.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `stat notas.txt` — o relatório completo de um ficheiro.
- `stat -c '%s %n' *` — tamanho e nome, uma linha cada.
- `stat -L ligacao` — descrever o que a ligação nomeia, não a ligação.
- `stat -f .` — o volume que contém o directório de trabalho.

## EXIT STATUS

- `0` — cada operando foi descrito (ou a ajuda breve foi escrita).
- `1` — pelo menos um operando não pôde ser lido, ou a saída falhou.
- `2` — a linha de comandos não foi compreendida, ou o seu formato nomeou
  uma directiva que este sistema não pode servir.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

ls, readlink, df, du
