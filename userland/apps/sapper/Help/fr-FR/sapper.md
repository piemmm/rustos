## NAME

sapper — déminer le terrain sans toucher de mine

## SYNOPSIS

`sapper`

## DESCRIPTION

Ouvre une fenêtre de bureau contenant une grille de cases couvertes. Certaines
cachent une mine. Chaque case découverte qui n'en cache pas indique combien de
ses huit voisines en cachent une, et ces nombres suffisent à déduire où sont les
mines. Découvrez toutes les cases sans mine et la partie est gagnée ; découvrez-
en une avec mine et elle est perdue.

La première case découverte est toujours sûre, ainsi que les huit qui
l'entourent : un premier coup ne peut donc jamais perdre et ouvre toujours une
zone à partir de laquelle raisonner.

Cliquez sur une case couverte pour la découvrir. Cliquez avec le bouton
secondaire pour y planter un drapeau, encore une fois pour un point
d'interrogation s'ils sont activés, et encore une fois pour l'effacer. Une case
marquée d'un drapeau est protégée : cliquer dessus ne fait rien.

Une fois une case ouverte, son nombre peut travailler pour vous. Cliquez sur un
nombre dont les drapeaux correspondent déjà et toutes les voisines restantes
sont découvertes d'un coup. Cliquez dessus avec le bouton secondaire lorsque ses
voisines couvertes sont exactement aussi nombreuses que son nombre et elles sont
toutes marquées d'un coup. Le bouton du milieu fait comme le premier, depuis
n'importe quel point de la case.

Le compteur de gauche indique combien de mines restent à trouver, moins les
drapeaux posés ; il devient négatif si vous posez plus de drapeaux qu'il n'y a de
mines. L'horloge de droite démarre à votre première case découverte et s'arrête
à la fin de la partie. Le bouton entre les deux lance une nouvelle partie, et son
visage dit comment s'est terminée la précédente.

Les flèches déplacent un anneau sur la grille. `Space` découvre la case qui s'y
trouve, ou l'accorde si elle est déjà ouverte. `F` marque la case, `Shift+F`
marque toutes ses voisines couvertes, `N` lance une nouvelle partie, et `1`, `2`
et `3` choisissent les grilles débutant, intermédiaire et expert. Les mêmes
choix, et le réglage des points d'interrogation, sont dans le menu de la barre
d'icônes du jeu.

Votre meilleur temps sur chacune des trois grilles standard est conservé d'une
session à l'autre. Une grille à vos propres dimensions n'en conserve aucun, car
deux grilles de ce genre ne sont jamais la même partie.

Le jeu se lance depuis la bibliothèque de programmes du bureau, sous Games, ou
par son nom depuis un interpréteur de commandes. Il exige une session graphique
en cours : sans elle le canal de fenêtre est inaccessible et le jeu signale le
refus sur le flux d'erreur standard puis se termine.

## EXIT STATUS

Zéro après une fermeture propre ; non nul lorsque le canal de fenêtre ou la
région de trame partagée a été refusé (la raison est indiquée sur le flux
d'erreur standard).
