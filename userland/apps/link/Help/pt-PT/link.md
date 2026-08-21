## NAME

link — dar a um ficheiro um segundo nome

## SYNOPSIS

`link [--] existente novo`

## DESCRIPTION

Cria uma ligação forte: `novo` passa a ser um segundo nome do nó que
`existente` já nomeia. Os dois nomes alcançam então o mesmo ficheiro — uma
escrita por um é visível pelo outro, porque há um ficheiro e não uma cópia
— e o armazenamento do ficheiro sobrevive até que o último dos seus nomes
seja removido.

Não há deliberadamente opções. O `ln` é a ferramenta com `-f`, `-i`, `-v`,
`-s`, `-L`/`-P` e as formas de destino `-t`/`-T`; mantê-las separadas
significa que um guião que tenha de criar uma só ligação forte e mais nada
dispõe de uma ferramenta que não pode substituir um nome, seguir uma
ligação nem criar uma simbólica em seu lugar.

Nenhum dos nomes é seguido. `existente` é o nó **tal como é escrito**,
pelo que uma ligação simbólica aí colocada não pode desviar o novo nome
para o seu destino (`ln -L` é a ferramenta para a postura que segue).
`novo` é um nome a criar: um nome ocupado é recusado, nunca substituído.

Cada recusa diz algo diferente:

- o novo nome já existe — uma criação nunca substitui um nome;
- `existente` é um **directório** — um directório tem exactamente um nome
  em qualquer parte, pelo que nenhum principal lhe pode dar um segundo;
- os dois nomes estão em **volumes diferentes** — o segundo nome de um nó
  tem de residir no volume que o armazena;
- a contagem de nomes por nó do formato transbordaria;
- o sistema de ficheiros guarda **um nome por nó** — uma propriedade
  permanente desse formato, não uma falha passageira. Aí use `ln -s` para
  uma ligação simbólica.

São exigidos exactamente dois operandos; qualquer outra coisa é um erro de
utilização e nenhuma ligação é criada. `--` termina a análise de opções.

## OPTIONS

- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `link relatorio.txt relatorio-copia.txt` — um segundo nome para um
  ficheiro.
- `link -- -nome-estranho segundo` — ligar um nome que começa por hífen.

## EXIT STATUS

- `0` — a ligação foi criada (ou a ajuda breve foi escrita).
- `1` — o sistema de ficheiros recusou a ligação, ou a saída falhou; a
  razão é impressa no erro padrão.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
