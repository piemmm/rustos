## NAME

servicectl — démarrer, arrêter, inscrire et désinscrire les services système

## SYNOPSIS

`servicectl [-h | -?] start|stop|enable|disable SERVICE`

## DESCRIPTION

Demande au gestionnaire de services de changer l'état d'exécution d'un
service enregistré, via son point de terminaison de contrôle contrôlé par
capacité. C'est le gestionnaire qui décide : cet outil encode seulement la
demande et rapporte la réponse.

Atteindre le point de terminaison constitue en soi l'autorisation. Sans
`CAP_SERVICE_CONTROL` dans le plafond de votre compte, le noyau refuse
l'appel avant que le gestionnaire ne le voie ; un compte non privilégié ne
peut donc même pas demander.

- `start SERVICE` — démarrer maintenant un service enregistré actuellement
  arrêté. Les conditions de disponibilité qu'il exige s'appliquent
  toujours : un service dont les conditions ne sont pas remplies est
  refusé plutôt que lancé dans un système qui ne peut pas le soutenir.
- `stop SERVICE` — arrêter proprement un service en cours, ainsi que ses
  dépendants dans l'ordre inverse des dépendances. Le service est invité à
  se terminer et n'est forcé qu'après son délai de grâce.
- `enable SERVICE` — enregistrer le service comme inscrit, afin que le
  gestionnaire le démarre à chaque amorçage, et le démarrer maintenant.
- `disable SERVICE` — l'enregistrer comme non inscrit, afin qu'aucun
  amorçage ultérieur ne le démarre, et l'arrêter maintenant.

En cas de succès, une ligne nomme l'état dans lequel le gestionnaire a
laissé le service.

Les deux sortes de changement affectent tous les principaux de la machine,
pas seulement votre session. `start` et `stop` ne modifient que le système
*en cours d'exécution*, si bien qu'un service inscrit revient au prochain
démarrage ; `enable` et `disable` modifient l'inscription même et lui
survivent donc.

## OPTIONS

- `-h, -?` — afficher l'aide courte de cette commande et quitter.
- `--` — terminer les options, afin qu'un service dont le nom commence par
  un tiret puisse tout de même être nommé.

## EXIT STATUS

- `0` — l'opération a été appliquée, ou l'aide courte a été affichée.
- `1` — le gestionnaire a refusé l'opération, ou le point de terminaison de
  contrôle n'a pas pu être atteint.
- `2` — la ligne de commande n'a pas été comprise ; rien n'a été envoyé.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide courte (une étiquette BCP-47 telle que
  `fr-FR`).

## SEE ALSO

- `ps`
- `man`
