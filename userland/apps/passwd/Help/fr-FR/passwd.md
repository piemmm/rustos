## NAME

passwd — définir le mot de passe d'un compte

## SYNOPSIS

`passwd [--record RECORD] [--] NAME`

## DESCRIPTION

Remplace le mot de passe enregistré du compte nommé. Définir un mot de passe
est une opération d'administration : la base refuse un appelant dépourvu de
la capacité d'administration des utilisateurs.

Aucun mot de passe en clair ne traverse l'appel système. L'outil demande
deux fois avec l'écho du terminal coupé, hache la saisie en un
enregistrement PBKDF2 salé — le sel venant de la source aléatoire du noyau —
et envoie l'enregistrement ; les deux tampons en clair sont mis à zéro dès
qu'il existe.

Le nom du compte est obligatoire. Le `passwd` de GNU sans opérande change le
mot de passe de l'appelant, ce qui exigerait sur TAIRiX un chemin
libre-service non privilégié qui n'existe pas : toute l'interface
d'administration des comptes est protégée par capacité, et y découper «
votre propre enregistrement » serait un changement du modèle de sécurité,
non une commodité.

Un appelant sans terminal — un programme graphique, dont l'entrée standard
est fermée sous le courtier d'élévation — hache lui-même le mot de passe et
transmet l'enregistrement fini avec `--record`, si bien qu'aucun clair
n'existe d'un côté ni de l'autre. L'enregistrement est vérifié comme bien
formé avant d'être stocké.

`--` termine l'analyse des options : tout argument ultérieur est un opérande.

## OPTIONS

- `--record RECORD` — un enregistrement PBKDF2 salé déjà prêt, pour un
  appelant sans terminal où demander.
- `-h, -?, --help` — afficher l'aide courte propre à cette commande.

## EXAMPLES

- `passwd ada` — demander deux fois et définir le mot de passe du compte.

## EXIT STATUS

- `0` — le mot de passe a été remplacé.
- `1` — la base a refusé ou échoué le remplacement, les deux saisies
  diffèrent, aucun aléa n'était disponible, ou l'enregistrement était mal
  formé ; la raison est écrite sur la sortie d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle que `fr-FR`).

## SEE ALSO

- `useradd`
- `usermod`
- `userdel`
- `users`
