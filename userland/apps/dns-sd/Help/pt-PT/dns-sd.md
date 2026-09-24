## NAME

dns-sd — explorar, resolver e procurar serviços da ligação local

## SYNOPSIS

`dns-sd [-t seconds] -B [type [domain]]`

`dns-sd [-t seconds] -L instance type [domain]`

`dns-sd [-t seconds] -G v4|v6|v4v6 host`

## DESCRIPTION

Pergunta à rede local — a ligação — pelos serviços que oferece, através do
serviço de descoberta da ligação local do sistema. O `dns-sd` nunca fala DNS
multicast por si: o serviço de descoberta pergunta ao segmento em seu nome e
admite cada pedido segundo a autoridade própria do programa.

Com `-B` explora as instâncias de um tipo de serviço, como `_ipp._tcp` para
impressoras, e mostra cada instância quando é adicionada ou removida. Sem
tipo, lista todos os tipos de serviço que a ligação oferece. Com `-L` resolve
uma instância no anfitrião e na porta onde é alcançada e mostra o que a
instância diz de si própria (os seus atributos `TXT`). Com `-G` procura os
endereços de um anfitrião sob `local`.

Cada resposta indica a interface em que foi aprendida, e uma linha `Flush`
significa que tudo o que foi aprendido nessa interface deixou de ser
conhecido — a sua ligação caiu, ou o serviço recomeçou. Cada nome mostrado foi
escolhido por outra máquina da ligação, pelo que é mostrado com escape, na
forma de apresentação DNS: um espaço como `\032`, um carácter de controlo pelo
seu código decimal.

Explorar todos os tipos, ou um tipo não concedido à conta em uso, requer
`CAP_NET_DISCOVER_ALL`, que só uma conta de administrador tem. O único domínio
da ligação é `local`.

## OPTIONS

- `-B` — explorar as instâncias de um tipo de serviço, ou todos os tipos se
  nenhum for dado.
- `-L` — resolver uma instância de um tipo de serviço.
- `-G` — procurar os endereços IPv4 (`v4`), IPv6 (`v6`) ou ambos (`v4v6`) de
  um anfitrião.
- `-t` — parar após este número de segundos em vez de correr até ser
  interrompido.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `dns-sd -B _ipp._tcp` — as impressoras da ligação, à medida que vêm e vão.
- `dns-sd -t 5 -B` — todos os tipos de serviço vistos em cinco segundos.
- `dns-sd -L "Hall Printer" _ipp._tcp` — onde uma impressora é alcançada.
- `dns-sd -G v4v6 printer.local` — os endereços de um anfitrião.

## EXIT STATUS

- `0` — o comando chegou ao fim (ou a ajuda breve foi escrita).
- `1` — o serviço de descoberta recusou o pedido ou não está a funcionar.
- `2` — a linha de comandos não foi entendida, ou a saída não pôde ser
  escrita.

## ENVIRONMENT

- `LANG` — o idioma preferido para a ajuda breve (uma etiqueta BCP-47 como
  `fr-FR`).

## SEE ALSO

- `host`
- `ping`
- `man`
