## NAME

lspci — listar os dispositivos PCI/PCIe descobertos

## SYNOPSIS

`lspci [-n | -nn] [-v] [-t] [-d [<vendor>]:[<device>]] [-s <node>]`

## DESCRIPTION

Lista, uma linha por função PCI/PCIe descoberta, o identificador do nó
da árvore de hardware da função, a sua classe e os nomes do fabricante
e do dispositivo. O inventário é a árvore de hardware — o inventário
único de dispositivos do sistema — lida através da API de informação
do sistema, que exige a capacidade `CAP_SYSINFO_HW`; uma recusa é
comunicada no erro padrão e nada é listado no seu lugar.

Os nomes provêm do instantâneo verificado da base pública de
identificadores PCI que este comando transporta no seu próprio pacote.
Uma identidade que a base não nomeia é mostrada na forma numérica
(`Vendor 8086`, `Device 2922`, `Class 0106`), nunca inventada, e o
número de tais dispositivos é anotado no fluxo de informação padrão
(fd 3). Se a tabela incluída faltar ou falhar a validação, a listagem
degrada para identificadores numéricos com a razão no erro padrão — o
inventário em si continua a ser listado.

O TAIRiX não regista um endereço PCI `bus:device.function`: o endereço
estável de uma função é o identificador do seu nó da árvore de
hardware, mostrado como `#<node>`, e `-s` seleciona esse identificador
(uma divergência deliberada e documentada face ao `lspci` do Linux). A
vista `-k` (controlador do núcleo) ainda não é oferecida: o sistema
não publica registos de vinculação de controladores, e o `lspci`
apenas comunica o que o sistema realmente regista.

## OPTIONS

- `-n` — apenas identificadores numéricos: o código de classe e
  `vendor:device` em hexadecimal.
- `-nn` — os nomes seguidos dos identificadores numéricos entre
  parênteses retos.
- `-v` — após cada função, listar os recursos que o seu nó declara
  (janelas MMIO, linhas IRQ, portas de E/S, restrições DMA) — os
  pedidos de concessão registados, não estado em tempo real.
- `-t` — representar as funções como árvore sob os barramentos pais.
- `-d [<vendor>]:[<device>]` — listar apenas as funções que
  correspondam aos identificadores dados (hexadecimal); uma metade
  omitida corresponde a qualquer valor.
- `-s <node>` — listar apenas a função com o identificador de nó dado
  (decimal).
- `-?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `lspci` — cada função PCI descoberta, com nomes.
- `lspci -nn` — o mesmo, com os identificadores numéricos ao lado.
- `lspci -v -s 7` — a linha do nó 7 mais os recursos declarados.
- `lspci -d 1af4:` — cada função do fabricante `1af4` (virtio).
- `lspci -t` — as funções na sua topologia de barramentos.

## EXIT STATUS

- `0` — a listagem (ou a ajuda curta) foi escrita.
- `1` — a consulta da árvore de hardware foi recusada ou falhou, ou a
  saída não pôde ser escrita.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `pt-PT`).

## SEE ALSO

- `sysinfo`
- `man`
