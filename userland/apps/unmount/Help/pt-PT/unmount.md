## NAME

unmount — desanexar um volume montado

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

Retira de serviço o volume montado sob `name`: o sistema de ficheiros
e o dispositivo são despejados, a montagem sob `/Storage` é retirada
e a raiz durável `id::` do volume é revogada. `name` é o nome de
catálogo do volume (`usb1`) ou o seu ponto de montagem
(`/Storage/usb1`), comparado com a lista de montagens da API de
informação do sistema.

Um volume cujo dispositivo foi removido ainda com escritas por
confirmar permanece visível como `unavailable-dirty` (ou
`unavailable-lost`), e um `unmount` simples recusa: os dados retidos
são guardados para uma reinserção verificada. `--force` é a saída
deliberada — os dados retidos são descartados, o volume é retirado e
a perda fica registada no registo de auditoria. Num volume saudável,
`--force` continua a despejar e desanexar de forma limpa; nada é
descartado quando uma confirmação limpa é possível.

Desanexar exige a autoridade de montagem (`CAP_FS_MOUNT`); o núcleo
verifica-a e audita cada decisão. Os volumes de arranque permanentes
e as ligações de vista do sistema não são desanexáveis.

## OPTIONS

- `-f, --force` — desmontagem forçada: retirar o volume mesmo quando
  os seus dados não podem ser confirmados, descartando os dados
  retidos.
- `-?, --help` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `unmount usb1` — desanexar de forma limpa o volume montado como
  `usb1`.
- `unmount /Storage/usb1` — o mesmo, indicado pelo ponto de montagem.
- `unmount --force usb1` — retirar um volume indisponível descartando
  os seus dados retidos.

## EXIT STATUS

- `0` — o volume foi desanexado (ou a ajuda curta foi escrita).
- `1` — o volume não foi encontrado, não é desanexável ou o núcleo
  recusou a desanexação.
- `2` — a linha de comandos não foi compreendida.

## ENVIRONMENT

- `LANG` — a região preferida para a ajuda curta (uma etiqueta BCP-47
  como `pt-PT`).

## SEE ALSO

- `mount`
- `df`
- `man`
