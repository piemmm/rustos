## NAME

servicectl — iniciar, parar, activar e desactivar serviços do sistema

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

## DESCRIPTION

Pede ao gestor de serviços que altere o estado de execução de um serviço
registado, através do seu ponto terminal de controlo protegido por
capacidade. É o gestor que decide: esta ferramenta apenas codifica o pedido
e reporta a resposta.

Alcançar o ponto terminal é, em si, a autorização. Sem
`CAP_SERVICE_CONTROL` no tecto da sua conta, o núcleo recusa a chamada
antes de o gestor a ver; uma conta sem privilégios não pode sequer
perguntar.

- `start SERVICE` — levantar agora um serviço registado que está parado. As
  condições de prontidão que exige continuam a aplicar-se: um serviço cujas
  condições não estão satisfeitas é recusado em vez de arrancado num
  sistema que não o pode sustentar.
- `stop SERVICE` — parar graciosamente um serviço em execução, e os seus
  dependentes em ordem inversa de dependência. Pede-se ao serviço que
  termine e só é forçado após o seu período de graça.
- `enable SERVICE` — registar o serviço como inscrito, para que o gestor o
  arranque em cada arranque, e arrancá-lo agora.
- `disable SERVICE` — registá-lo como não inscrito, para que nenhum arranque
  posterior o inicie, e pará-lo agora.

Em caso de sucesso, uma linha nomeia o estado em que o gestor deixou o
serviço.

Ambos os tipos de alteração afectam todos os principais da máquina, não
apenas a sua sessão. `start` e `stop` alteram só o sistema *em execução*, pelo
que um serviço inscrito regressa no próximo arranque; `enable` e `disable`
alteram a própria inscrição e por isso sobrevivem-lhe.

## OPTIONS

- `-h, -?` — mostrar a ajuda breve deste comando e sair.
- `--` — terminar as opções, para que um serviço cujo nome começa por um
  hífen possa ainda ser nomeado.

## EXIT STATUS

- `0` — a operação foi aplicada, ou a ajuda breve foi mostrada.
- `1` — o gestor recusou a operação, ou o ponto terminal de controlo não
  pôde ser alcançado.
- `2` — a linha de comandos não foi compreendida; nada foi enviado.

## ENVIRONMENT

- `LANG` — a localização preferida para a ajuda breve (uma etiqueta BCP-47 como
  `fr-FR`).

## SEE ALSO

- `ps`
- `man`
