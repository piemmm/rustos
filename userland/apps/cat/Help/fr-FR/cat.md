## NAME

cat — concaténer des fichiers vers la sortie standard

## SYNOPSIS

`cat [-AbeEnstTuv] [--] [file...]`

## DESCRIPTION

Lit chaque opérande de fichier dans l'ordre et écrit ses octets sur la
sortie standard. L'opérande `-` désigne l'entrée standard, et sans
opérande l'entrée standard est l'unique source.

Un opérande peut aussi être une référence de ressource typée comme
`sys:random` : elle est ouverte par le résolveur de ressources du
système (contrôlé par capacités) plutôt que par le système de fichiers —
`cat sys:random` produit des octets aléatoires. Une référence `info:`,
`state:` ou `stats:` nomme une valeur système typée plutôt qu'un flux ;
elle est lue via le service d'informations système, donc
`cat info:mem/physical` affiche cette valeur, et une lecture non
autorisée est refusée en nommant la capacité requise. Une référence mal
formée dans un espace de noms enregistré est une erreur, jamais un repli
vers un nom de fichier.

Avec `-n`, les lignes de sortie sont numérotées en continu sur toutes
les sources, de sorte qu'une ligne à cheval sur deux sources n'est
numérotée qu'une seule fois, à l'apparition de son premier octet.
`-b` ne numérote que les lignes non vides et l'emporte sur `-n`.
`-s` supprime les lignes vides adjacentes répétées ; une ligne
supprimée n'est ni écrite ni numérotée.

Les options de marquage rendent visibles les octets invisibles : `-E`
imprime `$` avant chaque saut de ligne, `-T` imprime TAB sous la forme
`^I`, et `-v` imprime les autres octets de contrôle sous la forme `^X`
et les octets non ASCII en notation `M-`. `-e`, `-t` et `-A` sont les
combinaisons habituelles `-vE`, `-vT` et `-vET`.

Une source qui ne peut pas être lue arrête la commande avant qu'une
source ultérieure ne soit touchée ; les octets déjà écrits le restent.

## OPTIONS

- `-A, --show-all` — équivalent à `-vET`.
- `-b, --number-nonblank` — numéroter les lignes de sortie non vides ;
  l'emporte sur `-n`.
- `-e` — équivalent à `-vE`.
- `-E, --show-ends` — imprimer `$` à la fin de chaque ligne.
- `-n, --number` — numéroter les lignes de sortie, en continu sur
  toutes les sources.
- `-s, --squeeze-blank` — supprimer les lignes vides adjacentes
  répétées.
- `-t` — équivalent à `-vT`.
- `-T, --show-tabs` — imprimer les caractères TAB sous la forme `^I`.
- `-u` — accepté et ignoré ; la sortie est déjà non tamponnée.
- `-v, --show-nonprinting` — utiliser la notation `^` et `M-` pour les
  octets de contrôle et non ASCII, sauf le saut de ligne et TAB.
- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `cat notes.txt` — écrire `notes.txt` sur la sortie standard.
- `cat a.txt - b.txt` — écrire `a.txt`, puis l'entrée standard, puis
  `b.txt`.
- `cat -n log.txt` — numéroter chaque ligne de sortie.
- `cat -bs draft.txt` — numéroter les lignes non vides et compacter
  les suites de lignes vides.
- `cat -A config.txt` — rendre visibles les fins de ligne, les
  tabulations et les octets de contrôle.
- `cat -- -n` — écrire le fichier nommé `-n`.

## EXIT STATUS

- `0` — chaque source a été écrite.
- `1` — une source n'a pas pu être lue, ou la sortie n'a pas pu être
  délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `ls`
- `man`
