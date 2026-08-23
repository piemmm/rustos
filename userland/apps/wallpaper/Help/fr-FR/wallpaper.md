## NAME

wallpaper — sélecteur d'arrière-plan de bureau graphique

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Ouvre une fenêtre de bureau proposant les fonds d'écran fournis avec le
système, la couleur d'arrière-plan derrière eux, et la manière dont le
bureau organise les icônes sur son tableau d'affichage. Rien ne change
à l'écran tant que les paramètres ne sont pas appliqués.

La fenêtre est pilotée par la souris. Un grand aperçu en haut montre le
fond d'écran sélectionné tel que le bureau le dessinera, avec la couleur
d'arrière-plan choisie partout où l'image n'atteint pas. En dessous, la
galerie liste chaque fond d'écran fourni sous forme de tuile : cliquez
sur l'une d'elles pour la sélectionner et l'aperçu suit immédiatement.
La tuile **No wallpaper** (Aucun fond d'écran), toujours en premier,
montre la couleur d'arrière-plan choisie seule.

La galerie défile lorsqu'elle contient plus de tuiles que la fenêtre n'en
affiche. Tournez la molette n'importe où sur la fenêtre, faites glisser
le curseur de la barre de défilement sur le bord arrière, ou cliquez sur
la piste au-dessus ou en dessous du curseur pour vous déplacer d'une
page à la fois.

À côté de l'aperçu se trouvent quatre paramètres, chacun étant une liste
déroulante. Cliquez sur l'un d'eux pour l'ouvrir et cliquez sur un choix
pour le valider :

- **Fit** (Ajustement) — comment l'image est placée : `fill` (couvrir
  l'écran, en recadrant le surplus), `fit` (la contenir entièrement,
  couleur d'arrière-plan dans les barres), `stretch` (déformer à la
  taille exacte de l'écran), `centre` (taille native, centrée), et
  `tile` (répéter à partir du haut à gauche).
- **Backdrop** (Arrière-plan) — la couleur unie affichée partout où le
  fond d'écran n'atteint pas : `Theme` suit le thème de bureau actif,
  et les couleurs nommées sont fixes. Une couleur déjà en vigueur qui
  n'est pas l'une des couleurs nommées est proposée sous son propre
  orthographe `rrggbb`.
- **Icons** (Icônes) — le coin du tableau d'affichage à partir duquel la
  grille d'icônes du bureau se développe.
- **Sort** (Tri) — l'ordre dans lequel les icônes du dossier du bureau
  sont listées.

L'aperçu est un modèle à l'échelle de votre écran : il a la même forme
que l'affichage, et montre l'image, l'arrière-plan et l'ajustement
sélectionnés exactement comme le bureau les affichera. Ce que vous
voyez dans l'aperçu est ce que vous obtenez.

Les images de fond d'écran ne sont jamais décodées par ce programme.
Chacune est rendue par un processus sandboxé séparé qui ne détient
aucune autorité sur le système de fichiers, le réseau ou le lancement,
de sorte qu'une image malformée ne peut pas compromettre le sélecteur ou
le bureau. Un fichier qui ne peut pas être décodé est marqué
`unreadable` dans sa tuile et n'est pas tenté à nouveau.

Le clavier permet d'accéder à tout ce que fait la souris. `Tab` et
`Shift-Tab` déplacent le focus vers l'avant et vers l'arrière à travers
la galerie, les quatre paramètres et les deux boutons. Les touches
fléchées permettent de se déplacer dans la galerie, ou d'ouvrir la liste
du paramètre focalisé et de s'y déplacer. `Enter` applique, ou active le
bouton focalisé, et `Escape` ferme la fenêtre sans appliquer.

L'application envoie les paramètres choisis à la session de bureau, qui
décide de les adopter ou non, redessine le tableau d'affichage et les
enregistre pour la prochaine connexion. Ce programme n'écrit jamais les
paramètres lui-même. Le résultat est rapporté à côté des boutons :
appliqué, refusé avec la raison de la session, ou aucune session de
bureau à l'écoute. Un refus laisse la fenêtre ouverte avec les choix
intacts.

Seul le magasin de fonds d'écran fournis est proposé ; une image
ailleurs sur le système ne peut pas être choisie depuis cette fenêtre.

## EXIT STATUS

Zéro après une fermeture propre, y compris lorsque les paramètres ont
été refusés. Non nul lorsque la fenêtre n'a pas pu être ouverte, que la
région de trame partagée a été refusée ou que le canal de fenêtre a été
perdu ; la raison est indiquée sur le flux d'erreur standard.

## ENVIRONMENT

Aucune. Les paramètres sur lesquels la fenêtre s'ouvre sont ceux que la
session de bureau publie elle-même, lus via le service de données
d'application plutôt que depuis un chemin nommé par ce programme ; c'est
la session qui les écrit, jamais ce programme.

## SEE ALSO

`files`, `viewer`
