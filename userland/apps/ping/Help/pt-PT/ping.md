## NAME

ping — enviar pedidos de eco ICMP a um host de rede

## SYNOPSIS

`ping [option...] endereco`

## DESCRIPTION

Envia pedidos de eco ICMP (IPv4) ou ICMPv6 (IPv6) a um host e mostra cada
resposta com o seu tempo de ida e volta, seguido de um resumo final.

Os pedidos passam por um socket de eco ICMP aberto na pilha de rede em
espaço de utilizador, protegido por `CAP_NET` e `CAP_NET_RAW` e auditado.
A pilha detém o identificador de eco, pelo que um socket só recebe
respostas aos seus próprios pedidos. Nesta versão não há resolução de
nomes, por isso o destino tem de ser um endereço IPv4 ou IPv6 literal; um
nome de host é um erro de utilização, não uma falha silenciosa.

Por predefinição, `ping` envia um pedido por segundo até ser
interrompido; `-c` limita a quantidade. Cada resposta indica a origem, o
número de sequência e o tempo; um pedido sem resposta dentro do prazo
imprime uma linha de expiração. O resumo final indica os pacotes
transmitidos e recebidos, a percentagem de perda e os tempos de ida e
volta mínimo, médio e máximo. `-q` mostra apenas o cabeçalho e o resumo.

O time-to-live IP não é exposto pela interface do socket de eco; ao
contrário de algumas implementações de `ping`, uma linha de resposta não
tem, por isso, um campo `ttl=`.

## OPTIONS

- `-c, --count` — parar após enviar esta quantidade de pedidos.
- `-i, --interval` — segundos entre pedidos (um decimal, p. ex. `0.5`).
- `-s, --size` — tamanho da carga útil em bytes.
- `-W, --timeout` — segundos de espera por cada resposta.
- `-w, --deadline` — prazo global da execução, em segundos.
- `-4, --ipv4` — exigir um destino IPv4.
- `-6, --ipv6` — exigir um destino IPv6.
- `-n, --numeric` — saída numérica. Sempre ativa no TAIRiX; aceite por
  familiaridade.
- `-q, --quiet` — silencioso: apenas o cabeçalho e o resumo final.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `ping 10.0.2.2` — fazer ping a um host IPv4 até ser interrompido.
- `ping -c 4 fe80::1` — enviar quatro pedidos a um host IPv6.
- `ping -c 10 -i 0.2 10.0.0.1` — dez pedidos, um a cada 200 ms.
- `ping -q -c 100 10.0.0.1` — execução silenciosa, apenas o resumo.

## EXIT STATUS

- `0` — foi recebida pelo menos uma resposta (ou a ajuda breve foi escrita).
- `1` — nenhum pedido obteve resposta.
- `2` — linha de comandos não compreendida, ou socket impossível de abrir.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda breve (uma etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

- `ss`
- `sysinfo`
- `man`
