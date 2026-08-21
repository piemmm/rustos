## NAME

readlink — imprimir o destino de uma ligação simbólica

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] ficheiro...`

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

As opções de canonização do GNU `-f`, `-e` e `-m` são **recusadas**, não
aproximadas. Resolver cada componente de um caminho — seguir cada ligação,
tratar `..` fisicamente, aplicar o orçamento de saltos e a regra de que
uma ligação não pode sair do volume que a guarda — é a única
implementação do sistema de ficheiros. Uma segunda cópia aqui poderia
imprimir um caminho que o sistema de ficheiros resolve de outro modo, pelo
que a opção falha até que o sistema de ficheiros ofereça essa resolução.

## OPTIONS

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
- `readlink -z a b | tr '\0' '\n'` — destinos separados por NUL para um
  guião.

## EXIT STATUS

- `0` — o destino de cada operando foi impresso (ou a ajuda breve foi
  escrita).
- `1` — pelo menos uma leitura foi recusada, ou a saída falhou.
- `2` — a linha de comandos não foi compreendida, ou nomeou uma opção de
  canonização.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
