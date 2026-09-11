## NAME

view — visionneuse graphique d'images et de documents

## SYNOPSIS

`view`

## DESCRIPTION

Ouvre une fenêtre de bureau affichant une image ou un document. Lancée avec
un document — depuis le gestionnaire de fichiers, ou en ouvrant une image —
elle affiche ce fichier ; lancée seule, elle demande au sélecteur de
fichiers de confiance de la session de bureau d'en choisir un.

La visionneuse ne détient aucune capacité sur le système de fichiers : elle
ne peut rien ouvrir, lister ni lire par elle-même. La session navigue pour
son compte sous sa propre identité, et seul le fichier choisi par
l'utilisateur lui est délégué — une seule fois et en lecture seule. Le
fichier n'est jamais décodé dans la visionneuse elle-même : ses octets sont
transmis à un processus de travail distinct qui n'a aucun accès au système de
fichiers, de sorte qu'un fichier malformé ou hostile ne peut atteindre rien
de ce que la visionneuse peut atteindre.

Les formats pris en charge sont JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO et
RISC OS Sprite. Un fichier que le décodeur refuse énonce sa raison dans la
fenêtre et sur la sortie d'erreur standard ; la fenêtre n'est jamais laissée
vide et aucune image n'est jamais fabriquée.

Plusieurs documents à la fois, ce sont plusieurs visionneuses : comparer deux
images côte à côte, c'est ouvrir la seconde. Fermer une fenêtre met fin à sa
visionneuse.

La barre d'outils en haut porte, dans l'ordre : réduire, agrandir, ajuster à
la fenêtre, taille réelle, entrée précédente, entrée suivante, tourner à
gauche, tourner à droite, miroir, lire ou suspendre une animation, et le
panneau d'informations. Un curseur de zoom continu se trouve à son bord
final. La ligne d'état en bas énonce le nom du document, son format, sa
taille en pixels, l'entrée affichée, sa longueur et le grossissement.

Faites glisser l'image pour vous y déplacer lorsqu'elle est plus grande que
la fenêtre ; des barres de défilement apparaissent au bord du canevas tant
qu'elle l'est. Tournez la molette au-dessus de l'image pour la déplacer. Un
appui secondaire sur l'image ouvre le menu de la visionneuse, dessiné par la
session de bureau.

La transparence est montrée sur un damier, afin qu'une image transparente se
lise comme transparente et non comme la couleur derrière elle.

* `+` — agrandir d'un cran
* `-` — réduire d'un cran
* `0` — ajuster toute l'image à la fenêtre
* `1` — taille réelle, un pixel d'image par pixel d'écran
* `2` — ajuster la largeur de l'image
* `[` / `]` — un quart de tour à gauche ou à droite
* `M` — miroir de gauche à droite
* `I` — afficher ou masquer le panneau d'informations
* `Space` — lire ou suspendre une animation
* `O` — choisir un autre document
* `Page Up` / `Page Down` — entrée précédente ou suivante
* `Home` / `End` — première ou dernière entrée
* touches fléchées — se déplacer dans l'image
* `Escape` — fermer la fenêtre

## OPTIONS

`-h`, `-?`, `--help`
: Écrire cette aide sur la sortie standard et quitter.

## EXIT STATUS

Zéro après une fermeture propre. Non nul lorsque le canal de fenêtre, la
région de trame partagée ou la session de bureau a été refusée ; la raison
est énoncée sur la sortie d'erreur standard.
