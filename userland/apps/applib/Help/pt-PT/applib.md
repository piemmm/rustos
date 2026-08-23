## NAME

applib — administrar a biblioteca de programas do ambiente de trabalho

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

Administra a biblioteca de programas — o catálogo organizado em pastas
de aplicações executáveis que o lançador do ambiente de trabalho
apresenta. A biblioteca são dados no volume, nunca uma lista compilada:
um armazenamento para toda a máquina em
`/System/Settings/ProgramLibrary/library.conf` que cada conta lê, mais
uma sobreposição opcional por utilizador que este comando guarda nas
suas próprias definições, onde só ele pode escrever e qualquer aplicação
pode ler o que ele publica. O que um lançador mostra é o resultado da
resolução de ambos em conjunto: as entradas e ajustes do próprio
utilizador prevalecem sobre os de toda a máquina.

Sem subcomando (ou com `list`), a biblioteca resolvida é impressa
pasta a pasta, uma entrada por linha: identificador, nome de exibição
e caminho do pacote — exatamente o que o lançador mostra. As pastas
são o conjunto fechado `Accessories`, `Graphics`, `Internet`,
`Multimedia`, `Office`, `Programming`, `Games`, `SystemTools`,
`Utilities` e `Other`; não existem pastas de formato livre.

`applib add` regista um pacote de aplicação. A sua identidade, nome de
exibição, pasta e ícone são obtidos a partir do próprio manifesto
`AppInfo` assinado do pacote; `--category`, `--name` e `--icon`
substituem o manifesto. Um pacote cujo manifesto não declare nenhuma
pasta de biblioteca necessita de uma `--category` explícita — a
ferramenta nunca adivinha. `applib remove` remove um registo, nomeado
pelo seu identificador ou pelo caminho do pacote com que foi registado.

`applib hide` suprime uma entrada da biblioteca resolvida sem remover
o seu registo — o seu identificador permanece reclamado, para que um
`rescan` posterior não possa ressuscitá-lo — e `applib show`
volta a mostrá-la. Ocultar é uma questão de apresentação, nunca de
autoridade: o lançamento de um pacote continua a ser regido pelas
verificações de assinatura e capacidades (capabilities) do carregador,
independentemente do catálogo.

`applib rescan` percorre os armazenamentos de aplicações
(`/System/Commands`, `/System/Applications` e `/Apps`, ou os próprios
`<home>/Commands` e `<home>/Applications` do chamador sob `--user`), lê
o manifesto de cada pacote e regista cada aplicação que solicite ser
listada e ainda não esteja catalogada. Os registos existentes —
incluindo renomeações e supressões de um curador — nunca são
perturbados, e um pacote com um manifesto ilegível ou malformado é
ignorado e contabilizado, nunca sendo motivo para abortar. É assim que
a biblioteca de um sistema novo se povoa a partir dos pacotes realmente
instalados, sem nenhuma lista mantida à mão em lado nenhum.

Por predefinição, a ferramenta edita o armazenamento de toda a
máquina, que apenas um principal admitido pela política de escrita de
`/System/Settings` pode alterar; uma conta comum lê-o, mas
personaliza-o através da sua própria sobreposição com `--user`. Uma
escrita recusada indica o seu motivo e não altera nada.

Em caso de sucesso, a ferramenta é silenciosa na saída padrão; o
resultado de uma alteração é emitido como um registo informativo
estruturado no fluxo de informação padrão (fd 3), que os scripts podem
capturar com `3>records.jsonl` e tudo o resto pode ignorar.

## OPTIONS

- `--category <folder>` — com `list`, mostrar apenas essa pasta; com
  `add`, arquivar a entrada sob a mesma (substituindo a declaração do
  manifesto).
- `--name <name>` — com `add`, o nome de exibição a mostrar em vez do
  nome no manifesto.
- `--icon <asset>` — com `add`, o recurso de ícone (um nome de ficheiro
  dentro do diretório `Resources/` do pacote) em vez do ícone no
  manifesto.
- `--user` — aplicar a alteração à própria sobreposição do utilizador
  (ou, com `rescan`, percorrer os próprios `<home>/Commands` e
  `<home>/Applications` do utilizador) em vez do armazenamento de toda
  a máquina.
- `-h, -?` — mostrar a ajuda curta deste comando.

## EXAMPLES

- `applib` — mostrar a biblioteca resolvida, pasta a pasta.
- `applib list --category Games` — mostrar uma única pasta.
- `applib add /Apps/chess.app` — registar um pacote conforme o seu
  manifesto solicita.
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  registar um pacote que não declara listagem, sob uma pasta explícita.
- `applib remove os.tairix.chess` — remover uma entrada por
  identificador.
- `applib hide os.tairix.chess --user` — ocultá-la apenas da sua
  própria biblioteca.
- `applib rescan` — registar cada pacote instalado e listado que ainda
  não esteja no catálogo da máquina.

## EXIT STATUS

- `0` — a listagem, alteração, rescan ou ajuda curta foram concluídos.
- `1` — uma falha de armazenamento, pacote ou saída (por exemplo, o
  utilizador não pode alterar o catálogo de toda a máquina); o motivo
  é indicado no fluxo de diagnóstico.
- `2` — a linha de comandos não foi compreendida, a pasta ou entrada
  é desconhecida, ou o pacote não pode ser registado conforme pedido.

## ENVIRONMENT

- `LANG` — o locale preferido para a ajuda curta (uma etiqueta BCP-47
  como `fr-FR`).
- `HOME` — o diretório pessoal do utilizador: as raízes de rescan com
  `--user` `<home>/Commands` e `<home>/Applications`. A sobreposição em si
  não precisa de um diretório pessoal; o serviço de definições resolve a
  conta a partir da identidade atestada pelo núcleo.

## SEE ALSO

- `man`
- `configure`
