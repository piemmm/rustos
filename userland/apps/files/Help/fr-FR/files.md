## NAME

files — navigateur de fichiers graphique

## SYNOPSIS

`files [--desktop] [répertoire] [-h | -?]`

## DESCRIPTION

Ouvre une fenêtre de bureau listant le système de fichiers, à partir du
`répertoire` nommé sur la ligne de commande, ou du répertoire personnel
de l'utilisateur qui a lancé le programme lorsqu'aucun n'est nommé. La
ligne du haut affiche le chemin du répertoire courant ; les lignes
suivantes listent les entrées du répertoire, l'entrée sélectionnée étant
surlignée avec la couleur d'accent du thème actif. Chaque lecture de
répertoire est un listage ordinaire, contrôlé par les permissions, sous
l'identité de l'utilisateur qui a lancé le programme : un répertoire
illisible est refusé, jamais deviné.

Le navigateur se lance depuis le bouton permanent `Files` de la barre
des tâches ou par son nom depuis un shell. Il exige une session
graphique en cours : sans elle, le canal de fenêtre est inaccessible et
le navigateur signale le refus sur le flux d'erreur standard puis se
termine.

La fenêtre se pilote au clavier : `Bas` et `Haut` déplacent la
sélection, `Entrée` ouvre le répertoire sélectionné et `Retour arrière`
remonte au répertoire parent. Fermer la fenêtre depuis le bureau met
fin au navigateur.

L'opérande `répertoire` est traité comme une entrée non fiable : ce doit
être un chemin absolu dans la limite de longueur de chemin du système,
et chacun de ses composants doit être un vrai nom de répertoire — `.` et
`..` n'en sont pas, si bien qu'une écriture ne peut jamais désigner
ailleurs que ce qu'elle donne à lire. Un répertoire qui enfreint une de
ces règles, ou que l'utilisateur qui a lancé le programme ne peut pas
lister, est refusé avec la raison sur le flux d'erreur standard et la
fenêtre s'ouvre alors sur le répertoire personnel, de sorte qu'un
mauvais argument ne laisse jamais l'utilisateur sans fenêtre. Un second
opérande est refusé d'emblée plutôt qu'ignoré.

## OPTIONS

- `--desktop` — s'exécuter comme le composant gestionnaire de fichiers du
  bureau lui-même : un emplacement permanent sur la barre d'icônes offrant
  vos lieux et les volumes montés, aucune fenêtre jusqu'à ce qu'on en demande
  une, et aucun moyen de quitter. La session de bureau passe cette option au
  démarrage ; nommer un `répertoire` avec elle est refusé, car un composant
  n'ouvre aucune fenêtre où le mettre.
- `-h, -?` — afficher la courte aide de cette commande et quitter.

## EXIT STATUS

Zéro après une fermeture propre, ou après l'affichage de la courte
aide ; `2` lorsque la ligne de commande n'a pas été comprise ; sinon non
nul lorsque le canal de fenêtre, la région de trames partagée ou le
listage initial du répertoire a été refusé (la raison est indiquée sur
le flux d'erreur standard).
