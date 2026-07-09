## NAME

lsusb — listar os dispositivos USB detetados

## SYNOPSIS

`lsusb [-v] [-t] [-d [<vendor>]:[<product>]] [-s [[<bus>]:][<devnum>]]`

## DESCRIPTION

Mostra, uma linha por interface USB detetada, os números de barramento
e de dispositivo da interface, o seu identificador `vendor:product` e
os nomes do fabricante e do produto. O inventário é a árvore de
hardware — o único inventário de dispositivos do sistema — lido através
da API de informações do sistema, que exige a capacidade
`CAP_SYSINFO_HW`; uma recusa é comunicada no erro padrão e nada é
listado no seu lugar.

Os nomes provêm do instantâneo verificado da base pública de
identificadores USB que este comando inclui no seu próprio pacote. Uma
identidade que a base não nomeia mostra apenas a sua forma numérica
`ID vvvv:pppp`, nunca inventada, e o número de tais dispositivos é
anotado no fluxo de informação padrão (fd 3). Se a tabela incluída
faltar ou falhar a validação, a listagem degrada para identificadores
nus com a razão no erro padrão — o inventário em si continua a ser
listado.

O RustOS não tem o registo Linux de números de barramento/dispositivo:
o número de barramento de um dispositivo é o identificador de nó
estável do seu controlador na árvore de hardware e o seu número de
dispositivo é o seu próprio identificador de nó, e `-s` seleciona
esses identificadores (uma divergência deliberada e documentada face
ao `lsusb` do Linux). O inventário regista um nó por *interface*: um
dispositivo com várias interfaces aparece uma vez por interface.

## OPTIONS

- `-v` — após cada dispositivo, listar a classe, a subclasse e o
  protocolo da interface (`bInterfaceClass`, `bInterfaceSubClass`,
  `bInterfaceProtocol`) com os nomes das tabelas de classes USB.
- `-t` — mostrar os dispositivos como uma árvore sob os seus
  controladores e barramentos.
- `-d [<vendor>]:[<product>]` — listar apenas os dispositivos que
  correspondam aos identificadores de fabricante/produto dados (hex);
  uma metade omitida corresponde a qualquer.
- `-s [[<bus>]:][<devnum>]` — listar apenas os dispositivos que
  correspondam aos identificadores de nó do controlador (barramento)
  e/ou do dispositivo (decimal); um valor sem dois pontos é um número
  de dispositivo sozinho.
- `-?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `lsusb` — cada dispositivo USB detetado, com nomes.
- `lsusb -v` — o mesmo, com a identidade de classe de cada interface.
- `lsusb -s 2:` — cada dispositivo sob o nó controlador 2.
- `lsusb -d 046d:` — cada dispositivo do fabricante `046d` (Logitech).
- `lsusb -t` — os dispositivos na sua topologia de barramento.

## EXIT STATUS

- `0` — a listagem (ou a ajuda curta) foi escrita.
- `1` — a consulta da árvore de hardware foi recusada ou falhou, ou a
  saída não pôde ser escrita.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `lspci`
- `sysinfo`
- `man`
