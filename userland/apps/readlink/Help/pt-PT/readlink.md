## NAME

readlink — imprimir o destino de uma ligação simbólica

## SYNOPSIS

`readlink [-fem] [-nz] [-q | -s | -v] [--] ficheiro...`

## DESCRIPTION

Imprime o destino que cada operando guarda, um por operando, na ordem da
linha de comandos.

O destino é impresso **tal como está guardado**. O destino de uma ligação
é um dado, não um caminho resolvido quando a ligação foi criada: pode ser
relativo, conter `..` e não nomear nada. Assim o `readlink` mostra a
escrita, e o `ls -l` mostra uma ligação ao lado daquilo que nomeia agora.

Um operando que **não** é uma ligação simbólica não tem destino a
imprimir — um ficheiro e um directório são ambos recusados com a mesma
razão «valor fora do intervalo» — e um nome ausente é «não encontrado».
Em ambos os casos os operandos restantes continuam a ser lidos e o comando
termina com estado diferente de zero. O silêncio é a predefinição, como na
ferramenta GNU: `-v` liga os diagnósticos por operando.

`-n` omite o delimitador após o último destino. Com mais de um operando é
ignorado, e isso é relatado, porque os delimitadores entre destinos são o
que os separa.

É exigido pelo menos um operando. `--` termina a análise de opções.

`-f`, `-e` e `-m` passam em vez disso à **canonização**: o único caminho
que nomeia aquilo em que o operando se resolve, com cada ligação seguida
e cada `..` aplicado. Com qualquer delas o operando não precisa de ser
uma ligação, e as três só diferem em quanto do caminho tem de existir.
São alternativas e não modificadores, pelo que vence a última dada.

Essa resolução é do sistema de ficheiros — `..` físico, o orçamento de
saltos, uma verificação da permissão de pesquisa em cada directório
atravessado e a regra de que uma ligação não pode resolver-se fora
daquilo que a sua montagem projecta — e esta ferramenta *invoca-a* em vez
de seguir ligações por si. Uma segunda cópia do algoritmo que divergisse
numa regra imprimiria um caminho que o sistema de ficheiros resolve de
outro modo.

## OPTIONS

- `-f, --canonicalize` — imprimir o caminho canónico; todos os
  componentes excepto o último têm de existir.
- `-e, --canonicalize-existing` — imprimir o caminho canónico; todos os
  componentes têm de existir.
- `-m, --canonicalize-missing` — imprimir o caminho canónico; nenhum
  componente tem de existir.
- `-n, --no-newline` — não imprimir o delimitador após o último destino
  (ignorado, com relato, para mais de um operando).
- `-z, --zero` — terminar cada destino com NUL em vez de nova linha.
- `-q, -s` — não diagnosticar uma leitura recusada (a predefinição;
  também `--quiet`, `--silent`).
- `-v, --verbose` — diagnosticar uma leitura recusada no erro padrão.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — imprimir o que um atalho guarda.
- `readlink -v alias` — imprimi-lo, e dizer porquê se não for uma
  ligação.
- `readlink -f alias` — imprimir aquilo em que se resolve, ligações
  incluídas.
- `readlink -z a b | tr '\0' '\n'` — destinos separados por NUL para um
  guião.

## EXIT STATUS

- `0` — o destino de cada operando foi impresso (ou a ajuda breve foi
  escrita).
- `1` — pelo menos uma leitura foi recusada, ou a saída falhou.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
