## NAME

ping — enviar pedidos de eco ICMP a um host de rede

## SYNOPSIS

`ping [option...] host`

## DESCRIPTION

Envia pedidos de eco ICMP (IPv4) ou ICMPv6 (IPv6) a um host e mostra cada
resposta com o seu tempo de ida e volta, seguido de um resumo final.

Os pedidos passam por um socket de eco ICMP aberto na pilha de rede em
espaço de utilizador, protegido por `CAP_NET` e `CAP_NET_RAW` e auditado.
A pilha detém o identificador de eco, pelo que um socket só recebe
respostas aos seus próprios pedidos.

O destino é um endereço IPv4 ou IPv6 literal ou um nome de host. Um nome é
resolvido pelo resolvedor de sistema, a partir dos servidores recursivos
configurados na máquina; um endereço literal não exige consulta alguma e
funciona por isso mesmo sem resolvedor configurado. Um nome que não
resolva para nenhum endereço da família pedida termina a execução
indicando a razão.

Por predefinição cada pedido transporta dados aleatórios de alta entropia,
sorteados de novo em cada pedido. É deliberado: uma ligação que comprime
ou desduplica o tráfego relataria de outro modo um débito e uma latência
que nada dizem da sua capacidade real. Os bytes devolvidos são comparados
com os enviados, pelo que uma carga aleatória serve também de verificação
de integridade por pacote. Use `-p` para um padrão fixo quando o que se
pretende é uma carga determinista.

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
- `-p, --pattern` — conteúdo da carga útil: `random` (predefinição, alta
  entropia) ou uma sequência de dígitos hexadecimais de comprimento par
  como padrão de bytes repetido, p. ex. `-p ff00`.
- `-W, --timeout` — segundos de espera por cada resposta.
- `-w, --deadline` — prazo global da execução, em segundos.
- `-4, --ipv4` — exigir um destino IPv4.
- `-6, --ipv6` — exigir um destino IPv6.
- `-n, --numeric` — saída numérica. Aceite e sem efeito: nunca é feita
  resolução inversa, pelo que os endereços de resposta já são numéricos.
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
- `2` — linha de comandos não compreendida, destino não resolvido, ou
  socket impossível de abrir.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda breve (uma etiqueta BCP-47
  como `fr-FR`).

## SEE ALSO

- `host`
- `ss`
- `sysinfo`
- `man`
