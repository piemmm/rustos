## NAME

host — resolver um nome por DNS

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

Resolve um nome de domínio nos seus endereços usando o resolvedor básico do
sistema e imprime cada resposta, uma por linha. Sem `-t`, são consultados
tanto os registos `A` (IPv4) como `AAAA` (IPv6); `-t type` restringe a
consulta a um.

Os servidores DNS recursivos a consultar são lidos da configuração do
anfitrião através da API de informação do sistema — o mesmo conjunto ativo
que a leitura `state:net/resolver/servers` apresenta — e cada resposta é
validada antes de mostrar um endereço. Não há `/etc/resolv.conf` nem ficheiro
de anfitriões local.

Um operando que é um endereço IPv4 ou IPv6 literal é uma pesquisa
**inversa**: é reescrito para o nome `in-addr.arpa` / `ip6.arpa` a que o
endereço corresponde, o tipo por omissão passa a `PTR`, e um registo
encontrado imprime-se como `<reverse-name> domain name pointer <name>.`

Apenas os registos `A`, `AAAA` e `PTR` são suportados; os outros tipos
(`MX`, `TXT`, etc.) são recusados em vez de tratados silenciosamente como
`A`. Um nome que não existe imprime `Host <name> not found: 3(NXDOMAIN)`;
quando nenhum servidor é alcançável, `host` reporta um tempo-limite esgotado
na saída de erro.

## OPTIONS

- `-t, --type` — o tipo de registo DNS a consultar: `A`, `AAAA` ou `PTR`
  (sem distinção de maiúsculas). Sem esta opção, um nome consulta `A` e
  `AAAA`, e um endereço consulta `PTR`.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `host example.com` — os endereços IPv4 e IPv6 do nome.
- `host -t AAAA example.com` — apenas os endereços IPv6.
- `host 10.0.2.2` — o nome a que esse endereço remete.

## EXIT STATUS

- `0` — foi encontrado pelo menos um endereço (ou a ajuda breve foi escrita).
- `1` — o nome não resolveu nenhum endereço (resposta negativa, tempo-limite
  esgotado ou falha do resolvedor).
- `2` — a linha de comandos não foi compreendida, ou a saída não pôde ser
  escrita.

## ENVIRONMENT

- `LANG` — a definição regional preferida para a ajuda breve (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
