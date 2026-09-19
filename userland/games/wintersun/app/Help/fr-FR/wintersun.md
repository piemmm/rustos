## NAME

wintersun — parcourir un monde hivernal généré de façon procédurale

## SYNOPSIS

`wintersun`

## DESCRIPTION

Ouvre une fenêtre de bureau sur un monde généré : une vue de dessus d'un
terrain que la machine synthétise au lieu de le livrer, éclairé par un soleil
bas qui étire de longues ombres sur chaque pente.

Rien de ce monde n'est stocké sous forme d'images. Chaque matière dont le sol
est fait — neige, roche, gravier, lande, toundra — tient en quelques nombres
que le client transforme en texture au moment de dessiner, si bien que le
monde est identique sur toutes les machines et n'occupe presque rien sur le
disque. Les routes s'usent dans ce qu'elles traversent au lieu de se poser
dessus.

Le terrain qui n'a pas encore été généré est dessiné comme le vide qu'il est,
puis se remplit à mesure qu'il arrive. Le client dessine ce qu'il a plutôt que
de s'arrêter pour attendre : la fenêtre continue donc de répondre pendant que
le monde rattrape son retard.

Les touches fléchées ou `W`, `A`, `S`, `D` font marcher. Deux touches tenues
ensemble suivent la diagonale à la même vitesse, et deux touches opposées
s'annulent. La vue vous suit et s'arrête au bord du monde au lieu d'en sortir.
Les pentes trop raides à gravir et l'eau trop profonde à traverser vous
écartent.

`+` et `-` rapprochent et éloignent la vue, en cinq paliers, d'une cellule du
monde large de huit pixels à une cellule large de cent vingt-huit.

`F11` passe la fenêtre en plein écran et la rend ensuite à son état précédent :
une fenêtre agrandie revient agrandie. `Échap` la restaure. `Q` quitte.

Le client dessine sous un budget d'image. Quand il ne peut pas le tenir, il
abandonne du détail dans un ordre fixe — densité des particules, puis
résolution du tampon de lumière, puis détail des matières, puis les ombres,
puis la taille de rendu — et la fréquence d'images n'est jamais ce qui cède.
Chaque palier est rendu dès que les images sont confortables depuis un
moment. L'ordre est fixe pour que le résultat sur une machine lente soit
prévisible plutôt qu'une surprise.

Une fenêtre plus grande que ce que le rendu logiciel peut remplir est dessinée
en 2560×1440 au plus, puis agrandie à la taille de la fenêtre.

## EXIT STATUS

`0` quand vous quittez. Un état non nul indique sa raison sur la sortie
d'erreur : le monde n'a pas pu être généré, la fenêtre n'a pas pu être
ouverte, ou le canal d'événements de la session a été perdu.

## SEE ALSO

`sapper`
