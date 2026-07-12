## NAME

terminal — émulateur de terminal graphique

## SYNOPSIS

`terminal`

## DESCRIPTION

Ouvre une fenêtre de bureau hébergeant le shell par défaut de
l'utilisateur sur un écran de 80×24 caractères. Les touches tapées dans
la fenêtre active sont envoyées au shell ; tout ce que le shell écrit
(sortie standard comme erreur standard) est interprété via le
vocabulaire ANSI/VT partagé et dessiné avec la palette du thème actif.
Le terminal lui-même ne fait jamais d'écho : l'écho et l'édition de
ligne appartiennent au shell, exactement comme sur une console.

Le terminal se lance depuis le menu démarrer du bureau (l'entrée
`Terminal`) ou par son nom depuis un shell. Il requiert une session
graphique active : sans elle, le canal de fenêtre est inaccessible et
le terminal signale le refus sur le flux d'erreur standard puis se
termine.

La session se termine quand le shell quitte (par exemple avec `exit`)
ou quand la fenêtre est fermée depuis le bureau ; fermer la fenêtre
termine le shell par une fin de fichier sur son entrée.

## EXIT STATUS

Zéro après une fermeture propre ou la sortie du shell ; non nul quand
le shell n'a pas pu être hébergé ou quand le canal de fenêtre, la
région de trame partagée ou la boîte d'événements a été refusé (la
raison est indiquée sur le flux d'erreur standard).
