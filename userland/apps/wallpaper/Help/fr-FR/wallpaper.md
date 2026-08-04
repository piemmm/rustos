## NAME

wallpaper — sélecteur d'arrière-plan de bureau graphique

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Ouvre une fenêtre de bureau proposant les fonds d'écran fournis avec le
système, la couleur d'arrière-plan derrière eux, et la manière dont le
bureau organise les icônes sur son tableau d'affichage. Rien ne change
à l'écran tant que les paramètres ne sont pas appliqués.

La grille liste chaque fond d'écran fourni sous forme de vignette, plus
une entrée **No wallpaper** (Aucun fond d'écran) qui affiche la couleur
d'arrière-plan choisie seule. Chaque vignette est rendue selon
l'ajustement actuellement choisi, afin qu'un aperçu montre ce que le
bureau fera réellement de cette image. Un fichier qui ne peut pas être
décodé affiche une tuile d'espace réservé marquée avec son nom et n'est
pas tenté à nouveau.

Les images de fond d'écran ne sont jamais décodées par ce programme.
Chacune est rendue par un processus sandboxé séparé qui ne détient
aucune autorité sur le système de fichiers, le réseau ou le lancement,
de sorte qu'une image malformée ne peut pas compromettre le sélecteur
ou le bureau.

Les lignes d'options sous la grille sont :

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
- **Icons** (Icônes) — le côté du tableau d'affichage à partir duquel la
  grille d'icônes du bureau se développe.
- **Sort** (Tri) — l'ordre dans lequel les icônes du dossier du bureau
  sont listées.

La fenêtre est pilotée par le clavier. `Tab` et `Shift-Tab` déplacent le
focus vers l'avant et vers l'arrière à travers la grille, les lignes
d'options et les boutons. Les touches fléchées permettent de se
déplacer dans la grille de vignettes ou de changer l'option focalisée.
`Enter` active le bouton focalisé, et `Escape` ferme la fenêtre sans
appliquer.

L'application envoie les paramètres choisis à la session de bureau, qui
décide de les adopter ou non, redessine le tableau d'affichage et les
enregistre pour la prochaine connexion. Ce programme n'écrit jamais les
paramètres lui-même. Le résultat est rapporté sur la ligne d'état sous
les lignes d'options : appliqué, refusé avec la raison de la session,
ou aucune session de bureau à l'écoute. Un refus laisse la fenêtre
ouverte avec les choix intacts.

Seul le magasin de fonds d'écran fournis est proposé ; une image
ailleurs sur le système ne peut pas être choisie depuis cette fenêtre.
Les clics de pointeur ne sélectionnent rien.

## EXIT STATUS

Zéro après une fermeture propre, y compris lorsque les paramètres ont
été refusés. Non nul lorsque la fenêtre n'a pas pu être ouverte, que la
région de trame partagée a été refusée ou que le canal de fenêtre a été
perdu ; la raison est indiquée sur le flux d'erreur standard.

## ENVIRONMENT

`HOME` nomme le propre répertoire personnel de l'utilisateur, sous
lequel `Settings/Pinboard/pinboard.conf` est lu au démarrage afin que la
fenêtre s'ouvre sur les paramètres en vigueur. Ce document est écrit
par la session de bureau, jamais par ce programme. Sans `HOME`, la
fenêtre s'ouvre sur les valeurs par défaut.

## SEE ALSO

`files`, `viewer`
