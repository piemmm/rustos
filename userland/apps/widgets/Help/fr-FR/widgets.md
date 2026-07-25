## NAME

widgets — galerie de composants Reactive Alloy

## SYNOPSIS

`widgets`

## DESCRIPTION

Ouvre une fenêtre de bureau qui présente chaque composant graphique partagé de
TAIRiX dans son propre onglet : boutons, sélecteurs, contrôles de valeur,
champs de texte, contrôles de choix, collections, barres, surfaces de
rétroaction et contrôles de fenêtre. Chaque onglet montre plusieurs variantes
de sa famille — rôles, états et valeurs différents — afin que le comportement
complet de chaque composant soit visible et interactif au même endroit.

Changez d'onglet en cliquant sur la barre d'onglets ou avec les touches
`Left`, `Right`, `Home` et `End` et `Enter`. Cliquez sur un composant pour
interagir avec lui : un interrupteur bascule, un curseur se déplace, un champ
de texte reçoit le caret, une liste déroulante s'ouvre. Un composant cliqué
conserve le focus clavier ; les flèches, `Enter`, `Space` et les caractères
saisis le pilotent alors, tandis que `Tab` et `Shift+Tab` déplacent le focus
entre la barre d'onglets et les composants.

La galerie se lance depuis le menu démarrer du bureau ou par son nom depuis un
shell. Elle exige une session graphique en cours : sans elle, le canal de
fenêtre est inaccessible et la galerie signale le refus sur le flux d'erreur
standard puis se termine.

## EXIT STATUS

Zéro après une fermeture propre ; non nul lorsque le canal de fenêtre ou la
région de trames partagée a été refusé (la raison est indiquée sur le flux
d'erreur standard).
