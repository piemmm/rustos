## NAME

telnet — o cliente de terminal virtual de rede (RFC 854)

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

Abre uma ligação TCP a um anfitrião e encaminha-lhe o terminal: a saída do
anfitrião aparece na saída padrão, as teclas premidas vão para o anfitrião e o
carácter de escape (`^]` por omissão) abre o interpretador de comandos
`telnet>`. Sem anfitrião, o `telnet` arranca nessa linha de comandos e o
`open` liga.

É tanto a forma de alcançar um serviço orientado a linhas noutra máquina como
a forma de interrogar à mão qualquer serviço TCP: `telnet host 80` abre uma
ligação onde se pode escrever um pedido.

O anfitrião pode ser um nome ou um endereço IPv4/IPv6 literal. Um nome é
resolvido pelo resolvedor mínimo do sistema, que lê os servidores DNS
recursivos configurados através da API de informação do sistema. A porta é um
número: não existe base de dados de serviços, pelo que um *nome* de serviço é
um erro de utilização e não um regresso silencioso à porta 23.

A negociação de opções segue o RFC 855 com a disciplina sem ciclos do
RFC 1143, pelo que um par que se repete nunca faz o cliente repetir-se. As
opções implementadas são BINARY, ECHO, SUPPRESS GO AHEAD, STATUS, TIMING MARK,
TERMINAL TYPE, NAWS, TERMINAL SPEED, TOGGLE FLOW CONTROL, LINEMODE e
NEW-ENVIRON; qualquer outra é recusada, que é o que significa uma opção não
implementada. O LINEMODE (RFC 1184) está implementado por inteiro — a máscara
`MODE`, a tabela de caracteres locais (SLC) e `FORWARDMASK` — pelo que o
cliente edita a linha como o servidor pede, com os caracteres que o servidor
negoceia.

O tamanho da janela é comunicado por NAWS na ligação e sempre que muda. O
TAIRiX não tem sinal de redimensionamento, pelo que o tamanho é relido a cada
tecla premida; um redimensionamento chega ao anfitrião na tecla seguinte.

O `NEW-ENVIRON` divulga **apenas** as variáveis que se definem e exportam com
o comando `environ`; o cliente nunca envia o seu próprio ambiente. O `-a` e o
`-l` exportam um nome de acesso, e é a única coisa que uma invocação divulga
por si.

Dois comandos da ferramenta histórica faltam deliberadamente. Não há escape
para a consola `!`: a um programa que analisa dados de rede hostis não se dá a
autoridade de lançar uma consola. Não há `slc check`, porque o RFC 1184 não
lhe dá forma alguma no cabo distinta de `slc export`. A interface de sockets
não expõe dados urgentes de TCP, pelo que um Synch viaja como a Data Mark
sozinha. Quando a entrada padrão chega ao fim do ficheiro — uma invocação
redirecionada como `telnet host 80 < pedido` — fecha-se apenas o lado de envio
e a sessão continua a ler até o anfitrião remoto também fechar, pelo que a
resposta não é descartada como faz a ferramenta histórica.

## OPTIONS

- `-4, --ipv4` — ligar apenas por IPv4.
- `-6, --ipv6` — ligar apenas por IPv6.
- `-8, --binary` — pedir um caminho de dados de 8 bits em ambos os sentidos.
- `-L, --eight-bit-output` — pedir um caminho de 8 bits apenas na saída.
- `-E, --no-escape` — sem carácter de escape; tudo vai para o anfitrião.
- `-e, --escape <char>` — definir o carácter de escape (`^]`, `^A`, um único
  carácter, ou vazio para nenhum).
- `-a, --login` — exportar o nome de acesso da sessão por `NEW-ENVIRON`.
- `-l, --user <name>` — exportar `name` como nome de acesso (implica `-a`).
- `-b, --bind <address>` — associar este endereço local antes de ligar.
- `-d, --debug` — registar a negociação de opções na saída de erro.
- `-?, --help` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `telnet example.test` — abrir uma sessão na porta telnet atribuída.
- `telnet 10.0.2.2 25` — falar à mão com um serviço de correio.
- `telnet -6 fe80::2` — ligar apenas por IPv6.
- `telnet -l ada host` — oferecer `ada` como nome de acesso.
- `telnet -8 host` — pedir um caminho de 8 bits em ambos os sentidos.
- `telnet` e depois `open host` — ligar a partir da linha de comandos.

## EXIT STATUS

- `0` — a sessão realizou-se (qualquer que tenha sido o fim dado pelo
  anfitrião), ou a ajuda breve foi escrita.
- `1` — a sessão não foi possível: o anfitrião não foi resolvido, o socket foi
  recusado, ou o terminal não passou a modo cru.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `TERM` — comunicado ao anfitrião pela opção TERMINAL TYPE.
- `USER` — o nome de acesso que o `-a` exporta.
- `LANG` — a região preferida para a ajuda breve (uma etiqueta BCP-47 como
  `pt-PT`).

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
