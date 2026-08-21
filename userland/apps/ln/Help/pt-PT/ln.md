## NAME

ln — criar ligações entre ficheiros

## SYNOPSIS

`ln [-sLPdFfinvT] [-t dir] [--] target... [link_name]`

## DESCRIPTION

Cria uma ligação simbólica que nomeia cada alvo. Com um só operando a
ligação é criada no directório de trabalho com o próprio nome do alvo.
Com dois, o segundo operando é um directório a preencher se o for — ou
uma ligação para um, salvo com `-n` — e o nome da ligação caso
contrário. Com três ou mais, o último tem de ser já um directório.

O alvo é guardado **literalmente** e nunca é resolvido: pode ser
relativo, conter `..` e não nomear nada, pelo que uma ligação pode
legitimamente ficar pendente. A sua gramática é ainda assim verificada
antes de ser guardada, pelo que um alvo que nenhum resolvedor poderia
percorrer é recusado. Criar uma ligação não concede autoridade alguma
sobre o que ela nomeia — cada uso posterior é autorizado componente a
componente sob a sua própria identidade.

Um nome de ligação já ocupado é recusado a menos que `-f` ou `-i` mande
substituí-lo, e a substituição **remove** primeiro esse nome, para que
nada atravesse uma ligação já presente até ao que ela aponta. Um
directório nunca é substituído.

A primeira falha pára a execução antes de qualquer alvo seguinte; as
ligações já criadas mantêm-se. `--` termina a análise de opções: todo o
argumento posterior é um operando.

Sem `-s` a ligação é **fixa**: uma segunda entrada de directório para o
próprio inode do alvo. Ambos os nomes alcançam um só ficheiro, uma
escrita por um é visível pelo outro, e o armazenamento do ficheiro
subsiste até se remover o último nome. Ambos os nomes têm de estar num
só volume, e a um directório nunca se dá um segundo nome — é por a
árvore de ficheiros continuar a ser uma árvore que `..` designa o
directório por onde realmente se passou.

`-b`/`-S`
são recusadas porque não existe maquinaria de cópias de segurança, e
`-r` porque calcular um alvo relativo ao directório da ligação exige uma
resolução canonizante que este sistema não oferece — uma lexical
nomearia outro objecto assim que houvesse uma ligação envolvida.

## OPTIONS

- `-s, --symbolic` — criar ligações simbólicas em vez de fixas.
- `-L, --logical` — ligar fixamente aquilo que um alvo simbólico
  nomeia, em vez da própria ligação.
- `-P, --physical` — ligar fixamente o alvo tal como escrito, sem
  seguir uma ligação simbólica final. Predefinição.
- `-d, -F, --directory` — aceitar um operando directório. A ligação é
  recusada na mesma: nenhum utilizador pode dar a um directório um
  segundo nome.
- `-f, --force` — remover um nome de ligação existente e criar então a
  ligação.
- `-i, --interactive` — perguntar antes de remover um nome de ligação
  existente; só consente uma resposta que comece por `y`/`Y`. Ganha a
  última de `-f` e `-i`.
- `-n, --no-dereference` — tratar um destino que é uma ligação
  simbólica para um directório como o simples nome que também é, em vez
  de um directório onde criar as ligações.
- `-v, --verbose` — comunicar cada ligação criada como
  `'link' -> 'target'`.
- `-t dir, --target-directory=dir` — criar cada ligação em `dir`, que
  tem de ser já um directório. O valor segue anexado (`-tdir`,
  `--target-directory=dir`) ou como argumento seguinte.
- `-T, --no-target-directory` — tratar o destino como nome de ligação,
  nunca como directório a preencher; exactamente dois operandos. Não
  combinável com `-t`.
- `-h, -?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `ln -s /System/Commands/ls.app tools/ls` — ligar um nome a um pacote.
- `ln -s ../shared/notes.txt` — ligar `notes.txt` aqui a um alvo
  relativo.
- `ln -sv -t Links a.txt b.txt` — ligar ambos os ficheiros em `Links`,
  comunicando cada ligação.
- `ln -sfn /Storage/media Music` — reapontar uma ligação `Music`
  existente para um novo directório, substituindo a ligação em vez de
  ligar lá dentro.

## EXIT STATUS

- `0` — todas as ligações foram criadas (ou a ajuda breve foi escrita);
  uma pergunta `-i` recusada não é uma falha.
- `1` — qualquer outro caso, com o motivo na saída de erro. Uma linha de
  comandos não compreendida também termina com `1`.

## ENVIRONMENT

- `LANG` — a locale preferida para a ajuda breve (uma etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

- `ls`
- `cp`
- `rm`
