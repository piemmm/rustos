## NAME

configure — ler e definir a configuração do sistema no arranque

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

Lista, mostra e define as opções do repositório de configuração em
`/System/Settings/Configuration/system.conf`. Sem operandos, cada opção
é listada com o seu valor atual; com uma chave apenas, o seu valor é
mostrado; com uma chave e um valor, a opção é alterada.

O repositório reside no volume raiz cifrado e é lido pelos seus
consumidores depois de o sistema de ficheiros raiz ser desbloqueado;
uma alteração produz efeito no próximo arranque do seu consumidor
(`os.loginType`: o início de sessão do próximo arranque; os
comutadores `cache.*`: o desbloqueio do próximo arranque).

O conjunto de chaves é fechado: uma chave desconhecida, ou um valor
fora do conjunto de uma chave, é recusado com a indicação das escolhas
válidas e nada altera. Alterar uma opção reescreve o repositório na sua
forma canónica e exige acesso de escrita a `/System/Settings` — uma
conta comum pode ler as opções mas não alterá-las.

- `os.loginType` — `text` ou `graphical`: o tipo de sessão que o
  serviço de início de sessão inicia para um utilizador autenticado.
  `text` (a omissão) inicia a shell da conta — o ambiente de trabalho
  pode ainda ser iniciado a pedido com o comando `desktop`; `graphical`
  inicia diretamente a sessão de ambiente de trabalho após a
  autenticação quando existe um instalado, recuando para texto quando
  não existe.
- `cache.all` — `on` ou `off`: o comutador principal da cache. `on` (a
  omissão) deixa cada classe de cache abaixo seguir a sua própria
  opção; `off` é um teto que desativa toda a cache em memória
  independentemente das opções por classe.
- `cache.filesystem`, `cache.block`, `cache.transform`,
  `cache.semantic` — `auto` ou `off`: os comutadores por classe para as
  quatro caches de memória recuperáveis (as caches do sistema de
  ficheiros, do bloco de disco inteiro, do cluster descomprimido e do
  arranque de aplicações). `auto` (a omissão) deixa o gestor de pressão
  de memória governar a classe; `off` desativa-a por completo. Não há
  um `on` por classe: uma classe não pode ser forçada a ignorar a
  pressão de memória. Uma classe está efetivamente `off` sempre que
  `cache.all` estiver `off`.

Cada cache é um acelerador recuperável, nunca a fonte da verdade, por
isso desligar qualquer uma ou todas apenas torna mais lento o trabalho
afetado — nunca altera um resultado.

- `net.ipv4.enabled`, `net.ipv6.enabled` — `true` ou `false`: os
  interruptores das famílias de endereços a nível da pilha. Ambos são
  `true` por predefinição. Uma família desativada não vincula
  endereços, não responde a pacotes e recusa um socket dessa família
  com um erro tipado — nunca um descarte silencioso.
- `net.ipv6.privacy` — `true` ou `false`: se a pilha forma endereços
  IPv6 temporários (de privacidade) além do estável. `false` (a
  predefinição) usa apenas o endereço SLAAC estável.
- `net.tcp.syncookies` — `auto` ou `always`: a defesa contra
  inundações SYN. `auto` (a predefinição) mantém uma fila semiaberta
  limitada e recorre a cookies sem estado em caso de transbordo;
  `always` responde a cada pedido de ligação sem estado. Não há `off`
  — uma fila de ligações indefesa não é uma definição.
- `net.tcp.keepalive` — `true` ou `false`: se as ligações TCP enviam
  sondas de manutenção numa ligação inativa. `false` (a predefinição)
  nunca sonda nem fecha uma ligação inativa; `true` sonda um par
  inativo após o intervalo habitual e fecha a ligação se este deixar de
  responder.

A pilha de rede lê as definições `net.*`; uma alteração produz efeito
quando a pilha aplica novamente a sua configuração.

## OPTIONS

- `-h, -?` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `configure` — listar todas as opções.
- `configure os.loginType` — mostrar o tipo de sessão por omissão.
- `configure os.loginType graphical` — arrancar no início de sessão
  gráfico.
- `configure cache.all off` — desativar toda a cache em memória em todo
  o sistema.
- `configure cache.filesystem off` — desativar apenas a cache do
  sistema de ficheiros.

## EXIT STATUS

- `0` — a listagem, o valor, a ajuda breve ou a alteração foi
  concluída.
- `1` — o repositório não pôde ser lido ou escrito (por exemplo, quem
  chama não pode alterar as definições do sistema), ou a saída não pôde
  ser entregue.
- `2` — a linha de comandos não foi compreendida, a chave é
  desconhecida ou o valor está fora do conjunto da chave.

## ENVIRONMENT

- `LANG` — o idioma preferido da ajuda breve (uma etiqueta BCP-47 como
  `fr-FR`).

## SEE ALSO

- `man`
