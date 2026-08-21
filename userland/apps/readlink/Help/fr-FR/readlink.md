## NAME

readlink — afficher la cible d'un lien symbolique

## SYNOPSIS

`readlink [-nz] [-q | -s | -v] [--] fichier...`

## DESCRIPTION

Affiche la cible que stocke chaque opérande, une par opérande, dans
l'ordre de la ligne de commande.

La cible est affichée **telle qu'elle est stockée**. La cible d'un lien
est une donnée, non un chemin résolu à la création du lien : elle peut
être relative, contenir `..`, et ne désigner rien du tout. `readlink`
montre donc l'écriture, et `ls -l` montre un lien à côté de ce qu'il
désigne actuellement.

Un opérande qui n'est **pas** un lien symbolique n'a pas de cible à
afficher — un fichier et un répertoire sont tous deux refusés pour la
même raison « valeur hors limites » — et un nom absent est « introuvable ».
Dans les deux cas les opérandes restants sont lus et la commande termine
avec un état non nul. Le silence est le comportement par défaut, comme
dans l'outil GNU : `-v` active les diagnostics par opérande.

`-n` supprime le délimiteur après la dernière cible. Avec plus d'un
opérande il est ignoré, et cela est signalé, car les délimiteurs entre
cibles sont ce qui les sépare.

Au moins un opérande est requis. `--` termine l'analyse des options.

Les options de canonisation GNU `-f`, `-e` et `-m` sont **refusées**, non
approchées. Résoudre chaque composant d'un chemin — suivre chaque lien,
traiter `..` physiquement, appliquer le budget de sauts et la règle
qu'un lien ne peut sortir du volume qui le stocke — est l'unique
implémentation du système de fichiers. Une seconde copie ici pourrait
afficher un chemin que le système de fichiers résout autrement : l'option
échoue donc, jusqu'à ce que le système de fichiers offre cette résolution
lui-même.

## OPTIONS

- `-n, --no-newline` — ne pas afficher le délimiteur après la dernière
  cible (ignoré, avec un signalement, pour plus d'un opérande).
- `-z, --zero` — terminer chaque cible par NUL au lieu d'un saut de
  ligne.
- `-q, -s` — ne pas diagnostiquer une lecture refusée (par défaut ;
  aussi `--quiet`, `--silent`).
- `-v, --verbose` — diagnostiquer une lecture refusée sur la sortie
  d'erreur standard.
- `-?, --help` — afficher l'aide courte de cette commande.

## EXAMPLES

- `readlink Home:/Desktop/Notes` — afficher ce que stocke un raccourci.
- `readlink -v alias` — l'afficher, et dire pourquoi si ce n'est pas un
  lien.
- `readlink -z a b | tr '\0' '\n'` — cibles séparées par NUL pour un
  script.

## EXIT STATUS

- `0` — la cible de chaque opérande a été affichée (ou l'aide courte a
  été écrite).
- `1` — au moins une lecture a été refusée, ou la sortie a échoué.
- `2` — la ligne de commande n'a pas été comprise, ou nommait une option
  de canonisation.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

ln, link, unlink, ls
