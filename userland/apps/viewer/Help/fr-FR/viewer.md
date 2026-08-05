## NAME

viewer — visionneuse de fichiers graphique en lecture seule

## SYNOPSIS

`viewer`

## DESCRIPTION

Ouvre une fenêtre de bureau et demande immédiatement au sélecteur de
fichiers de confiance de la session de bureau de choisir un fichier.
La visionneuse ne détient aucune capacité de système de fichiers :
elle ne peut rien ouvrir, lister ni lire par elle-même. La session
navigue pour le compte de la visionneuse sous sa propre identité, et
seul le fichier choisi par l'utilisateur est délégué à la visionneuse
— à usage unique et en lecture seule.

Le contenu du fichier choisi est affiché en texte brut depuis le haut
de la fenêtre. Les caractères imprimables sont affichés tels quels ;
tout autre octet est représenté par un point, de sorte que le contenu
binaire apparaisse manifestement aseptisé. Le contenu affiché est
limité au début du fichier.

La fenêtre est pilotée à la souris. Cliquez sur le bouton **Open…**
(Ouvrir…) dans l'en-tête pour demander un autre fichier. Faites
glisser le curseur de la barre de défilement vers le haut ou vers le
bas pour parcourir un long fichier, cliquez sur sa piste au-dessus ou
en dessous du curseur pour changer de page, cliquez sur ses boutons
d'extrémité pour avancer d'une ligne, ou tournez la molette au-dessus
de la fenêtre pour faire défiler. Annuler le sélecteur laisse la
visionneuse ouverte avec un avis ; fermer la fenêtre depuis le bureau
termine la visionneuse.

Le clavier est une voie secondaire pour les mêmes actions : `Enter`
demande un autre fichier, les touches fléchées avancent d'une ligne,
Page Up/Page Down avancent d'une page, et Home/End sautent au début
ou à la fin.

## EXIT STATUS

Zéro après une fermeture propre ; non nul lorsque le canal de fenêtre
ou la région d'image partagée a été refusé (la raison est indiquée sur
le flux d'erreur standard).
