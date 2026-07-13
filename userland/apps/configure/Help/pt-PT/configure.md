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
(`os.loginType`: o início de sessão do próximo arranque).

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

## OPTIONS

- `-h, -?` — mostrar a ajuda breve deste comando.

## EXAMPLES

- `configure` — listar todas as opções.
- `configure os.loginType` — mostrar o tipo de sessão por omissão.
- `configure os.loginType graphical` — arrancar no início de sessão
  gráfico.

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
