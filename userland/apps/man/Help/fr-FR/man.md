## NAME

man — afficher le document d'aide d'une commande

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Affiche le document d'aide fourni par le paquet applicatif d'une commande,
dans votre langue lorsqu'une traduction existe.

Chaque programme TAIRiX est un paquet applicatif portant une arborescence
`Help/` : un document structuré par commande ou sujet, par langue. `man`
résout `<command>` exactement comme l'interpréteur de commandes — d'abord
le préfixe fixe de magasins `/System/Commands`, `/System/Applications`,
`<home>/Commands` et `<home>/Applications`, puis les répertoires de
`PATH` — la page affichée documente donc toujours le programme que
l'interpréteur lancerait pour le même mot ; `PATH` ne peut ni réordonner
ni remplacer ce préfixe. Un suffixe `.app` nomme directement le paquet.
Quand aucun d'eux ne contient le mot, `man` parcourt les magasins
d'applications récursivement — d'abord `/Apps`, puis vos propres dossiers
`Commands` et `Applications` de votre répertoire personnel — ainsi un
paquet rangé dans des dossiers imbriqués est tout de même trouvé ; la
recherche ne regarde jamais à l'intérieur d'un autre paquet, et la
correspondance la moins profonde l'emporte.

Le document est choisi selon la locale de la variable d'environnement
`LANG`, avec repli vers la même langue dans une autre région puis vers le
document canonique en anglais. Lorsque la page n'est pas affichée dans la
langue demandée, `man` signale la substitution sur le flux consultatif
(fd 3) ; la page elle-même ne mélange jamais les langues.

Sur une console interactive, la page est affichée écran par écran :
l'espace tourne la page, entrée avance d'une ligne et `q` arrête. Quand la
sortie est redirigée ou que la taille de la console est inconnue, la page
entière défile.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande.

## EXAMPLES

- `man ps` — afficher la page de `ps`.
- `man top keys` — afficher le sujet `keys` du paquet `top`.
- `man files.app` — nommer directement le paquet.

## EXIT STATUS

- `0` — la page a été affichée.
- `1` — la commande ou son document d'aide est introuvable, ou la page n'a
  pas pu être délivrée.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée (une étiquette BCP-47 telle que `fr-FR`).
- `PATH` — les répertoires supplémentaires où chercher les paquets
  `<command>.app`, après le préfixe fixe de magasins.
- `HOME` — nomme vos propres dossiers `Commands` et `Applications` pour
  la recherche récursive de paquets.

## SEE ALSO

- `elsh`
