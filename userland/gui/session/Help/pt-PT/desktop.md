## NAME

desktop — iniciar a sessão gráfica do ambiente de trabalho

## SYNOPSIS

`desktop`

## DESCRIPTION

Inicia a sessão gráfica do ambiente de trabalho no posto desta máquina:
o comando adquire o arrendamento exclusivo de ecrã e entrada do posto,
liga-se ao serviço de ecrã e executa o ambiente de trabalho composto —
o gestor de janelas e a barra de tarefas — até a sessão terminar. O
comando regressa quando a sessão do ambiente de trabalho termina.

O mesmo ambiente de trabalho arranca automaticamente após a
autenticação quando o administrador configurou um início de sessão
gráfico (`configure os.loginType graphical`); este comando inicia-o a
pedido a partir de uma shell de texto.

Quando nenhum serviço de ecrã está em execução, ou outra sessão já
detém o posto, o comando falha escrevendo o motivo na saída de erro
padrão — nunca desaloja uma sessão em curso.

## OPTIONS

- `-h, -?` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `desktop` — iniciar a sessão do ambiente de trabalho.

## EXIT STATUS

- `0` — a ajuda curta foi servida.
- `2` — a linha de comandos não foi compreendida.
- qualquer outro código diferente de zero — a sessão não pôde arrancar
  (sem posto, sem serviço de ecrã) ou terminou (o arrendamento do posto
  perdeu-se); o motivo é escrito na saída de erro padrão.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda curta (uma etiqueta
  BCP-47 como `fr-FR`).

## SEE ALSO

- `configure`
- `man`
