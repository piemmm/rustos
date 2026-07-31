## NAME

files — navigateur de fichiers graphique

## SYNOPSIS

`files`

## DESCRIPTION

Ouvre une fenêtre de bureau listant le système de fichiers, à partir de
la vue racine. La ligne du haut affiche le chemin du répertoire
courant ; les lignes suivantes listent les entrées du répertoire,
l'entrée sélectionnée étant surlignée avec la couleur d'accent du thème
actif. Chaque lecture de répertoire est un listage ordinaire, contrôlé
par les permissions, sous l'identité de l'utilisateur qui a lancé le
programme : un répertoire illisible est refusé, jamais deviné.

Le navigateur se lance depuis le bouton permanent `Files` de la barre
des tâches ou par son nom depuis un shell. Il exige une session
graphique en cours : sans elle, le canal de fenêtre est inaccessible et
le navigateur signale le refus sur le flux d'erreur standard puis se
termine.

La fenêtre se pilote au clavier : `Bas` et `Haut` déplacent la
sélection, `Entrée` ouvre le répertoire sélectionné et `Retour arrière`
remonte au répertoire parent. Fermer la fenêtre depuis le bureau met
fin au navigateur.

## EXIT STATUS

Zéro après une fermeture propre ; non nul lorsque le canal de fenêtre,
la région de trames partagée ou le listage initial du répertoire a été
refusé (la raison est indiquée sur le flux d'erreur standard).
