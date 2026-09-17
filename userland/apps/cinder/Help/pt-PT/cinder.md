## NAME

cinder — um companheiro de ambiente de trabalho que vive num parque e percorre o ambiente de trabalho

## SYNOPSIS

`cinder`

## DESCRIPTION

Abre uma pequena janela-parque com o Cinder lá dentro. O Cinder é a mascote do
TAIRiX: uma criatura pequena, cor de ferrugem e antracite, que vagueia, se
limpa, dormita e repara onde está o seu ponteiro.

Pode deixá-lo sair. Escolha **Deixar o Cinder sair** no menu dele na barra de
ícones e ele deixa o parque para ir para o próprio ambiente de trabalho, onde
passeia entre as suas janelas. Ao encontrar uma, trepa e senta-se na barra de
título, achata-se e esgueira-se por baixo, ou simplesmente contorna-a — o que
faz depende da forma da janela e da disposição dele. Aproxime o ponteiro e ele
fica a olhar, segue-o a trote e salta-lhe em cima se o apanhar.

Clique nele para lhe fazer festas, no parque ou cá fora. As festas animam-no. Cá
fora só ele recebe o clique: o espaço transparente à volta dele pertence ao que
estiver por trás, por isso um companheiro sentado por cima do seu trabalho nunca
engole um clique destinado a este.

No parque também o pode pegar ao colo e pousar em qualquer ponto do chão, e dar
toques na bola.

**O menu.** **Deixar o Cinder sair** / **Trazer o Cinder para casa** troca onde ele está.

**Sair** termina-o e retira-o do ambiente de trabalho.

**Fechar o parque.** Fechar a janela do parque não termina nada. O Cinder é uma aplicação residente:
fechar o parque com ele lá dentro arruma-o, e fechá-lo enquanto ele está cá fora
deixa-o a vaguear. Clique no ícone dele na barra para voltar a abrir o parque. É
*Sair* que o termina.

**Disposição.** O Cinder quer três coisas: descanso, brincadeira e companhia. Correr gasta-lhe a
energia e uma soneca repõe-na; estar parado aborrece-o e perseguir o ponteiro
diverte-o; as festas animam-no. Não tem de gerir nada disto — não há nada para
lhe dar de comer e nada corre mal se o deixar em paz. Está ali para que perceba
num relance em que disposição ele está.

Como se sente, e se estava cá fora, é recordado entre sessões.

**Quando não pode sair.** Estar no ambiente de trabalho fora de uma janela exige a capacidade
`CAP_DESKTOP_LAYER`, que esta aplicação pede no seu manifesto assinado e que as
permissões da sua conta também têm de permitir. Se não permitirem — ou se não
houver sessão gráfica — **Deixar o Cinder sair** indica o motivo no parque e no
erro padrão, e o parque continua a funcionar. O Cinder fica simplesmente lá
dentro.

## EXIT STATUS

`0` numa saída limpa. Um estado diferente de zero é sempre acompanhado do motivo
no erro padrão.

## SEE ALSO

`sapper`
