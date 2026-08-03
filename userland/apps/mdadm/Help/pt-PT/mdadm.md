## NAME

mdadm — inspecionar e administrar matrizes RAID

## SYNOPSIS

`mdadm --create --level=<level> --raid-devices=<count> [--chunk=<blocks>] <device>...`

`mdadm --detail [<array>]`

`mdadm --examine`

`mdadm --add <array> <device>`

`mdadm --remove <array> <device>`

`mdadm --stop <array>`

## DESCRIPTION

Inspeciona e administra as matrizes RAID por software que o compositor de
matrizes monta a partir dos dispositivos membros. O inventário de
matrizes e dispositivos é lido através da API de informação do sistema —
a mesma interface, ao mesmo nível `CAP_SYSINFO_HW` com que a árvore de
hardware é lida. As mutações de criar, adicionar, remover e parar são
enviadas ao ponto de controlo do compositor, que verifica que quem chama
detém `CAP_STORAGE_ADMIN` antes de agir. Uma recusa é comunicada na saída
de erro com um código de saída diferente de zero; nada é inventado e
nenhuma autoridade é presumida.

Indica-se exatamente um modo por invocação.

O TAIRiX não tem `/dev`, por isso os dois nomes que o Linux mdadm escreve
como ficheiros de dispositivo escrevem-se aqui de outra forma — uma
divergência deliberada e documentada:

- Um dispositivo é nomeado pelo identificador do seu nó na árvore de
  hardware, escrito `node:<id>`, o mesmo nome que os relatórios mostram.
  Qualquer outra grafia é recusada em vez de adivinhada.
- Uma matriz é nomeada pela sua identidade de 128 bits em hexadecimal.
  Aceita-se a identidade completa de 32 dígitos, tal como qualquer
  prefixo que nomeie exatamente uma matriz; um prefixo que corresponde a
  mais de uma matriz é recusado em vez de adivinhar qual se pretendia.

O TAIRiX compõe os níveis RAID 0, 1, 5, 6, 10 e a tripla paridade. Não
tem RAID4, por isso `--level=4` é recusado com essa razão.

Um contexto consultivo conciso — uma matriz degradada, ou dispositivos em
branco não mostrados na vista de matrizes — é escrito no fluxo de
informação padrão (fd 3). É opcional e nunca altera a saída principal.

## OPTIONS

- `-C, --create` — criar uma matriz sobre os dispositivos nomeados e
  imprimir a identidade que o compositor lhe atribui.
- `-D, --detail` — comunicar a identidade, o nível, a saúde, as
  contagens de dispositivos, a geometria e qualquer posição de
  reconstrução ou verificação de cada matriz. Sem operando de matriz,
  comunicar todas as matrizes.
- `-E, --examine` — listar todos os dispositivos que o compositor detém:
  os membros de matrizes com a sua ranhura e estado, e os dispositivos em
  branco não afiliados sobre os quais se pode criar uma nova matriz.
- `-a, --add` — admitir um dispositivo em branco numa ranhura ausente de
  uma matriz e reconstruí-lo.
- `-r, --remove` — retirar um dispositivo membro de uma matriz.
- `-S, --stop` — parar uma matriz ativa e libertar os seus membros.
- `-l, --level=<level>` — o nível a criar: `0`/`raid0`/`stripe`,
  `1`/`raid1`/`mirror`, `5`/`raid5`, `6`/`raid6`, `10`/`raid10`, ou
  `tp`/`raid-tp` para a tripla paridade.
- `-n, --raid-devices=<count>` — o número de ranhuras de membro a criar;
  deve ser igual ao número de operandos de dispositivo.
- `-c, --chunk=<blocks>` — a unidade de faixa em blocos lógicos; válida
  apenas para um nível com faixas.
- `-h, -?, --help` — mostrar a ajuda própria deste comando.
- `-V, --version` — imprimir a versão e sair.

## EXAMPLES

- `mdadm --create --level=raid5 --raid-devices=3 node:11 node:12 node:13` — criar uma matriz RAID5 sobre três dispositivos.
- `mdadm --detail` — comunicar todas as matrizes.
- `mdadm --examine` — listar todos os dispositivos, membros e em branco.
- `mdadm --add 3f2a node:14` — adicionar um dispositivo à matriz cuja identidade começa por `3f2a`.
- `mdadm --stop 3f2a` — parar essa matriz.

## EXIT STATUS

- `0` — o pedido teve sucesso (ou a ajuda foi escrita).
- `1` — uma capacidade foi recusada, um nome não foi resolvido, o
  compositor recusou o pedido, ou a saída não pôde ser escrita.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a configuração regional preferida para esta ajuda (uma
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `sysinfo`
- `man`
