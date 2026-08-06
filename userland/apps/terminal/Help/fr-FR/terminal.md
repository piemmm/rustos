## NAME

terminal — émulateur de terminal graphique

## SYNOPSIS

`terminal`

## DESCRIPTION

Ouvre une fenêtre de bureau hébergeant le shell par défaut de
l'utilisateur sur un écran de 80×25 caractères. Les touches tapées dans
la fenêtre active sont envoyées au shell ; tout ce que le shell écrit
(sortie standard comme erreur standard) est interprété via le
vocabulaire ANSI/VT partagé et dessiné avec la palette de couleurs
choisie dans les paramètres. Le terminal lui-même ne fait jamais d'écho :
l'écho et l'édition de ligne appartiennent au shell, exactement comme
sur une console.

La fenêtre s'ouvre aux dimensions que mesure l'écran 80×25 dans la
taille de texte en vigueur, afin qu'elle s'adapte à l'affichage sur
lequel elle est présentée ; sur un écran trop petit pour cette taille,
le texte est réduit plutôt que la fenêtre rétrécie, car un programme qui
se conçoit pour 80 colonnes doit toujours les obtenir.

Le terminal se lance depuis la Bibliothèque de programmes du bureau (le
bouton `Library` de la barre des tâches) ou par son nom depuis un
shell. Il requiert une session graphique active : sans elle, le canal de
fenêtre est inaccessible et le terminal signale le refus sur le flux
d'erreur standard puis se termine.

La session se termine quand le shell quitte (par exemple avec `exit`)
ou quand la fenêtre est fermée depuis le bureau ; fermer la fenêtre
termine le shell par une fin de fichier sur son entrée.

Appuyer sur le bouton secondaire (droit) de la souris n'importe où sur
l'écran ouvre le menu du terminal. Chaque ligne dispose d'un raccourci
clavier qui fonctionne que le menu soit ouvert ou non, et `Escape` — ou
un clic en dehors du menu — le ferme sans rien choisir.

| Ligne | Raccourci | Ce qu'elle fait |
| --- | --- | --- |
| Paramètres… | `Ctrl ,` | Ouvre les paramètres décrits ci-dessous. |
| Texte plus grand | `Ctrl +` | Dessine l'écran un pas plus grand. |
| Texte plus petit | `Ctrl -` | Dessine l'écran un pas plus petit. |
| Taille réelle | `Ctrl 0` | Revient à la taille de texte par défaut. |
| Effacer l'écran | `Ctrl Shift K` | Vide l'écran sans écrire dans le shell. |
| Fermer | `Ctrl Shift W` | Ferme la fenêtre et termine le shell. |

Les paramètres s'ouvrent dans la fenêtre elle-même et comportent deux
onglets. **Apparence** choisit la palette de couleurs, définit la taille
du texte et modifie la palette propre de l'utilisateur. Les palettes
fournies sont *System* (qui suit l'apparence sombre ou claire du
bureau), *Midnight*, *Phosphor*, *Amber*, *Ember*, *Contrast*, *Paper*
et *Custom*. Choisir *Custom* utilise les couleurs modifiées sous le
sélecteur : une grille des vingt couleurs dont un écran est composé — le
fond, le premier plan, le curseur, le texte du curseur et les seize
couleurs ANSI — avec des curseurs rouge, vert et bleu pour celle qui est
sélectionnée.

**Effets** définit la manière dont l'écran est dessiné.

| Effet | Ce qu'il fait |
| --- | --- |
| Opacité | La solidité de l'arrière-plan. En dessous du plein, le bureau transparaît derrière le texte, qui reste parfaitement lisible. |
| Flou d'arrière-plan | Le degré de flou du bureau derrière une fenêtre transparente. N'a aucun effet sur une fenêtre totalement opaque. |
| Lignes de balayage | Assombrit une ligne sur deux, l'aspect plat d'un masque d'ombre. |
| Bruit | Un bruit de fond par pixel en mouvement, comme celui d'un signal analogique. |
| Phosphore | La durée de persistance des pixels allumés, de sorte que le texte qui défile rapidement laisse une traînée. |
| Ondulation | Un lent vacillement horizontal mouvant, comme celui d'un tube déréglé. |

Chaque modification prend effet immédiatement et est enregistrée dans le
profil propre de l'utilisateur, `~/Settings/Terminal/terminal.conf`, de
sorte qu'un terminal ultérieur s'ouvre de la même manière. Le profil est
un simple fichier texte de lignes `clé valeur` avec `#` commençant un
commentaire, et peut être modifié à la main ; les couleurs y sont
écrites sous forme de six chiffres hexadécimaux bruts (`1b242e`), jamais
avec un `#` en tête, qui commencerait un commentaire. Un profil absent
signifie les valeurs par défaut ; un profil que le terminal ne peut pas
lire ou analyser signifie également les valeurs par défaut, et la raison
est indiquée sur le flux d'erreur standard.

## EXIT STATUS

Zéro après une fermeture propre ou la sortie du shell ; non nul quand
le shell n'a pas pu être hébergé ou quand le canal de fenêtre, la
région de trame partagée ou la boîte d'événements a été refusé (la
raison est indiquée sur le flux d'erreur standard).

## ENVIRONMENT

`HOME`
: Le répertoire personnel du compte, où le terminal lit et écrit son
profil. Sans lui, le terminal fonctionne sur le profil par défaut et
n'enregistre rien.

`TERM`
: Exporté vers le shell hébergé sous la valeur `xterm-256color`, qui
nomme l'émulateur que ce terminal présente. Toute valeur héritée est
remplacée ; le reste de l'environnement est transmis au shell tel quel.

## SEE ALSO

`elsh`, `sysinfo`
