## NAME

link — donner un second nom à un fichier

## SYNOPSIS

`link [--] existant nouveau`

## DESCRIPTION

Crée un lien physique : `nouveau` devient un second nom du nœud que
`existant` désigne déjà. Les deux noms atteignent alors le même fichier —
une écriture par l'un est visible par l'autre, car il y a un fichier et
non une copie — et le stockage du fichier survit jusqu'au retrait du
dernier de ses noms.

Il n'y a volontairement aucune option. `ln` est l'outil avec `-f`, `-i`,
`-v`, `-s`, `-L`/`-P` et les formes de destination `-t`/`-T` ; les garder
séparés signifie qu'un script qui doit créer un seul lien physique et rien
d'autre dispose d'un outil incapable de remplacer un nom, de suivre un
lien ou de créer un lien symbolique à la place.

Aucun des deux noms n'est suivi. `existant` est le nœud **tel qu'il est
écrit**, de sorte qu'un lien symbolique placé là ne peut pas rediriger le
nouveau nom vers sa cible (`ln -L` est l'outil pour la posture de suivi).
`nouveau` est un nom en cours de création : un nom occupé est refusé,
jamais remplacé.

Les refus disent chacun quelque chose de différent :

- le nouveau nom existe déjà — une création ne remplace jamais un nom ;
- `existant` est un **répertoire** — un répertoire a exactement un nom
  partout, donc aucun mandant ne peut lui en donner un second ;
- les deux noms sont sur des **volumes différents** — le second nom d'un
  nœud doit résider sur le volume qui le stocke ;
- le compteur de noms par nœud du format déborderait ;
- le système de fichiers stocke **un nom par nœud** — une propriété
  permanente de ce format, non une défaillance passagère. Utilisez
  `ln -s` pour un lien symbolique là.

Exactement deux opérandes sont requis ; tout autre nombre est une erreur
d'usage et aucun lien n'est créé. `--` termine l'analyse des options.

## OPTIONS

- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `link rapport.txt rapport-sauvegarde.txt` — un second nom pour un
  fichier.
- `link -- -nom-etrange second` — lier un nom commençant par un tiret.

## EXIT STATUS

- `0` — le lien a été créé (ou l'aide courte a été écrite).
- `1` — le système de fichiers a refusé le lien, ou la sortie a échoué ;
  la raison est affichée sur la sortie d'erreur standard.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

ln, unlink, readlink, ls
