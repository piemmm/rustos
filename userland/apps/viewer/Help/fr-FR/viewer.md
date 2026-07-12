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
tout autre octet est représenté par un point. Le contenu affiché est
limité au début du fichier.

Appuyez sur `Entrée` pour demander un autre fichier. Annuler le
sélecteur laisse la visionneuse ouverte avec un avis. Fermer la
fenêtre depuis le bureau termine la visionneuse.

## EXIT STATUS

Zéro après une fermeture propre ; non nul lorsque le canal de fenêtre
ou la région d'image partagée a été refusé (la raison est indiquée sur
le flux d'erreur standard).
