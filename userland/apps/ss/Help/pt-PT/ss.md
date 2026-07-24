## NAME

ss — listar os sockets abertos

## SYNOPSIS

`ss [option...]`

## DESCRIPTION

Lista os sockets abertos do sistema, uma linha por socket: o protocolo
de transporte, o estado da ligação, o enchimento das filas de receção e
de envio, a `address:port` local e remota e — com `-p` — o processo
proprietário.

As linhas provêm da lista de sockets da API de Informação do Sistema,
que a pilha de rede responde como consulta privilegiada e auditada:
nomeia os sockets de cada principal e o par de cada ligação, pelo que
listar todos os sockets exige `CAP_SYSINFO_GLOBAL`. Não há `/proc/net`;
a uma sessão sem essa capacidade isso é comunicado e o `ss` termina, em
vez de imprimir uma tabela vazia.

Por omissão a lista mostra os sockets ligados, não em escuta. `-l`
mostra apenas os sockets em escuta e `-a` ambos; a contagem de ouvintes
ocultos é anotada no fluxo de informação padrão (fd 3), nunca na tabela.
`-t` e `-u` restringem o protocolo e `-4`/`-6` a família de endereços;
sem nenhum, mostram-se todos os protocolos e famílias. As portas e os
endereços são sempre numéricos (o TAIRiX não tem base de nomes de
serviço), pelo que `-n` é aceite mas está sempre em vigor. Um endereço
não especificado imprime-se como `*` e uma porta não vinculada como `*`;
um endereço IPv6 fica entre parênteses retos para que o separador
`:port` permaneça sem ambiguidade.

`ss` aceita apenas opções. A gramática de expressões de filtro do
iproute2 (filtros de estado e de endereço) não está implementada, pelo
que um operando isolado é um erro de uso e não um argumento ignorado em
silêncio.

## OPTIONS

- `-t, --tcp` — mostrar os sockets TCP. Sem `-t` nem `-u`, mostram-se
  ambos os protocolos.
- `-u, --udp` — mostrar os sockets UDP.
- `-a, --all` — mostrar os sockets em escuta e ligados.
- `-l, --listening` — mostrar apenas os sockets em escuta.
- `-n, --numeric` — não resolver nomes de serviço. Sempre em vigor no
  TAIRiX; aceite por familiaridade.
- `-p, --processes` — acrescentar a coluna do processo proprietário
  (`pid=N`).
- `-4, --ipv4` — restringir a lista a sockets IPv4.
- `-6, --ipv6` — restringir a lista a sockets IPv6.
- `-H, --no-header` — suprimir a linha de cabeçalho.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `ss` — os sockets ligados, não em escuta.
- `ss -a` — cada socket, em escuta e ligado.
- `ss -l` — apenas os sockets em escuta.
- `ss -tlp` — os sockets TCP em escuta, com o processo proprietário.
- `ss -u4` — os sockets UDP sobre IPv4.

## EXIT STATUS

- `0` — a lista foi produzida (ou a ajuda breve foi escrita).
- `1` — a consulta de sockets foi recusada ou falhou, ou não foi
  possível escrever a saída.
- `2` — a linha de comando não foi compreendida.

## ENVIRONMENT

- `LANG` — a configuração regional preferida para a ajuda breve (uma
  etiqueta BCP-47 como `fr-FR`).

## SEE ALSO

- `ping`
- `sysinfo`
- `man`
