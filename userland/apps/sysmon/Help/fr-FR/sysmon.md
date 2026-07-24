## NAME

sysmon — observer en direct la mémoire, les caches et la charge du noyau

## SYNOPSIS

`sysmon [-d sec.dixièmes] [-h | -?]`

## DESCRIPTION

`sysmon` est une vue en direct, en plein écran, de ce que le noyau fait
de la mémoire et du processeur, lue entièrement via l'API d'information
système — il n'y a pas de `/proc` à racler. Elle montre la mémoire
physique et sa composition, le tas du noyau, la bande de pression mémoire
et son historique récent, le registre des caches récupérables avec les
**taux de succès** par classe, le palier compressé `ramzip`, le total de
mémoire épinglée, l'utilisation de stockage des volumes montés, la charge
par CPU, la table d'interruptions du noyau et un recensement des
processus. Elle reste utilisable pendant que le système est sous charge
délibérée et se met en veille entre les rafraîchissements au repos (la
lecture se gare ; elle ne tourne jamais à vide).

Au démarrage, le moniteur épingle sa propre mémoire (`mem_pin`, qui exige
`CAP_MEM_PIN`) afin de ne jamais caler sur ses propres défauts de page
sous la pression même qu'il observe. Un épinglage refusé est signalé sur
la ligne de titre et la session continue sans épinglage — l'épinglage est
accessoire, jamais fatal.

L'affichage se rafraîchit à chaque intervalle (3,0 secondes sauf si `-d`
le change). Le moniteur n'accepte aucun opérande : il se pilote par des
touches pressées dans la session.

- `q` — quitter.
- Gauche / Droite (ou `p`) — changer de panneau de détail (Gauche =
  précédent, Droite / `p` = suivant) : caches, palier compressé, stockage
  des volumes montés (disques), charge par CPU, lignes d'interruption,
  processus.
- `r` — rafraîchir maintenant.
- `+` / `-` — allonger / raccourcir l'intervalle d'une seconde, entre 0,1
  et 60 secondes.
- Haut/Bas, Page préc./Page suiv., Début/Fin — faire défiler le panneau
  ciblé.
- `h`, `?` — afficher ou masquer le pense-bête des touches de la session
  (qui reproduit la légende des barres ci-dessous).

### Le bloc de résumé

Un bloc de résumé fixe précède le panneau de détail. Chaque ligne est
étiquetée à gauche pour se lire sans couleur ; la couleur n'est qu'un
renfort.

- **Ligne de titre** — le nom de l'outil, la durée de fonctionnement du
  système (`up D days, H:MM`), les trois moyennes de charge (1/5/15
  minutes) et l'état d'épinglage (`[pinned]`, ou `[unpinned: <reason>]`
  quand l'épinglage a été refusé).
- **`Mem`** — la barre de mémoire (voir la légende des barres), suivie des
  Mio utilisés / totaux, du pourcentage utilisé, de la taille du tas du
  noyau et — quand ils sont non nuls — des chiffres du magasin compressé
  `ramzip` et de la mémoire épinglée `pinned`.
- **`Pres`** — la barre de pression mémoire : une jauge à cinq bandes,
  chaque bande atteinte remplie de sa propre couleur de gravité, suivie du
  nom de la bande courante, des chiffres libre / réserve et du total des
  entrées en bande.
- **`Hist`** — la bande d'historique de pression : un glyphe par
  rafraîchissement, le plus ancien à gauche, chacun coloré selon sa
  bande — `.` normale, `-` légère, `=` modérée, `#` sévère, `!` critique —
  de sorte qu'une plage de pression se lit comme une série colorée.
- **`CPU`** — la barre CPU globale (voir la légende des barres), suivie du
  pourcentage occupé toutes CPU, du nombre de CPU et des compteurs cumulés
  de changements de contexte et de préemptions.
- **`Tasks`** — le recensement des processus : totaux, en exécution,
  endormis, arrêtés et zombies (avec `(own)` ajouté quand le recensement
  de tous les processus a été refusé et que seules les tâches propres sont
  comptées).
- **Barre d'onglets des panneaux** — chaque panneau de détail, celui ciblé
  mis en évidence, avec un indicateur de défilement à droite quand le
  panneau ciblé déborde.

### La légende des barres

Les jauges `Mem` et `CPU` sont des barres entre crochets `[…]`. Le
pense-bête `?` reproduit cette légende dans la session en cours.

La barre de mémoire (`Mem`) est une barre **empilée** dont les cellules
nomment ce que contient la mémoire physique — une répartition *disjointe*
de la mémoire utilisée (`used` vaut `total` moins `free`), de sorte que
rien n'est compté deux fois et que la largeur remplie est exactement la
fraction utilisée :

- `#` — mémoire résidente utilisateur (vert) : pages résidentes dans les
  espaces d'adressage utilisateur.
- `K` — le tas du noyau (cyan) : les tas et dalles propres au noyau.
- `=` — autre mémoire en usage (magenta) : tout ce qui est utilisé mais
  non attribué ci-dessus (caches de pages, tampons, cadres du noyau).
- vide — mémoire libre.

Le magasin compressé `ramzip` et la mémoire anonyme `pinned` recouvrent
ces seaux (les pages épinglées sont résidentes utilisateur ; le magasin
compressé est de la mémoire noyau), aussi sont-ils rapportés en chiffres à
côté de la barre plutôt qu'en segments séparés qui compteraient double —
une comptabilité honnête plutôt qu'une image trompeuse.

La barre de pression (`Pres`) colore chaque bande selon sa profondeur :
normale/légère vert, modérée jaune, sévère/critique rouge.

La barre CPU (`CPU`) se remplit de cellules occupées `#` sur une piste
inactive vide, colorée selon la part occupée (vert sous 60 %, jaune sous
85 %, rouge à 85 % ou plus). TAIRiX comptabilise le temps CPU uniquement en
occupé contre inactif — il n'y a pas de répartition
utilisateur/système/e-s dans l'API — aussi la barre montre-t-elle une
seule catégorie honnête d'occupation, le détail par cœur figurant dans le
panneau `cpu`.

### Les panneaux de détail

Gauche / Droite (ou `p`) parcourt six panneaux. Chacun a un en-tête de
colonne inversé (vidéo inverse, gras) afin que le titre se lise comme une
barre distincte au-dessus du corps.

### caches — le registre des caches récupérables

Ce sont les caches que le noyau peut rendre pour soulager la pression
mémoire **sans perte de données** : chaque entrée est reconstructible
depuis sa source canonique, si bien que le noyau l'abandonne au lieu de la
pagineter. Le panneau répond directement à « les caches font-ils leur
travail ? » : chaque ligne est une classe de récupération, agrégée sur
tous les caches enregistrés, et porte son propre **taux de succès**.

Colonnes :

- `class` — la classe de récupération (voir la liste des classes plus
  bas).
- `entries` — entrées vivantes actuellement détenues pour la classe.
- `cached` — l'empreinte résidente de la classe : la charge utile des
  entrées plus les métadonnées de comptabilité par entrée, ensemble.
- `hits` — recherches de la classe servies depuis le cache depuis le
  démarrage (le cache a évité la source canonique).
- `misses` — recherches de la classe retombées sur la source canonique
  depuis le démarrage.
- `hit%` — le taux d'efficacité du cache, `hits / (hits + misses)` en
  pourcentage entier. Un taux élevé signifie que le cache rentabilise sa
  mémoire ; un taux bas, qu'il retient de la mémoire sans éviter de
  travail. Il affiche `-`, jamais un `0%` fabriqué, pour une classe que
  rien n'a recherchée durant ce démarrage (un dénominateur inactif).
- `ref` — admissions **refusées** depuis le démarrage (une entrée que le
  cache a décliné de détenir : hors budget, non comptabilisable, ou faute
  de mémoire).
- `shr` — passes de **rétrécissement** forcé par la pression ayant
  récupéré des entrées de la classe depuis le démarrage.
- `fail` — **échecs** internes attribués à la classe : un défaut de
  registre détecté ayant empoisonné (désactivé fail-closed) un cache.

Les décomptes s'abrègent au-delà de 99 999 en `k`/`M`/`G`/`T` (milliers
décimaux, pas des Kio) afin qu'une colonne ne s'élargisse jamais.

Les classes de récupération, dans l'ordre où le noyau les récupère sous
pression (la première listée est abandonnée en premier, de sorte qu'un
cache bas dans la liste survit le plus longtemps) :

- `disposable-ui` — état d'interface jetable (ressources rasterisées,
  atlas de glyphes, instantanés de fenêtre) : le moins coûteux à perdre,
  le premier à partir.
- `predictive-prefetch` — données préchargées spéculativement (listages,
  vignettes, index de complétion) : jamais nécessaires à la correction.
- `background-validation` — produits de travail de validation au repos
  (progression de balayage, empreintes candidates) : le travail
  spéculatif s'arrête dès que la pression commence.
- `semantic-app-cache` — état vérifié de lancement d'applications
  (manifestes analysés, résumés de validation, résultats de résolution de
  commandes). Le récupérer ne peut jamais rendre une application
  inlançable — la porte de chargement se rejoue simplement.
- `runtime-cache` — état dérivé détenu par le runtime (préparation du
  chargeur, cartes de ressources) : groupé avec le cache sémantique.
- `clean-file-data` — *contenu* de fichier propre et reconstructible,
  relisible depuis le volume : une lecture de périphérique bornée
  reconstruit un morceau. Récupéré avant que rien ne soit compressé dans
  `ramzip`.
- `transform-cache` — formes intermédiaires coûteuses de données
  autorisées (données de grappe vérifiées, déchiffrées, décompressées) :
  plus coûteuses à reconstruire qu'une lecture propre, donc récupérées
  après les données de fichier propres.
- `fs-metadata` — métadonnées du système de fichiers : enregistrements
  d'état, résultats de recherche de noms, entrées de répertoire et
  enregistrements de sécurité. Petites, chaudes et reconstruites seulement
  par un parcours d'arbre en plusieurs étapes, elles survivent donc aux
  données de fichier sous pression.
- `reliability-assist` — état reconstructible d'assistance à la
  récupération (fenêtres de vérification, résumés de santé) : justifié par
  la latence de récupération, il est donc préservé le plus longtemps.

### ramzip — le palier de mémoire compressée

`ramzip` compresse les pages anonymes froides dans un magasin plus petit
en RAM au lieu de les pagineter. Ses sections :

- `tier` — l'empreinte vivante : `entries` détenues, octets `logical`
  (non compressés) représentés, octets `stored` (chiffrés) réellement
  détenus et octets `metadata` de comptabilité ; puis `saved` (logique
  moins stocké) avec son pourcentage du logique — la mémoire que le palier
  récupère.
- `capacity` — les plafonds dérivés auxquels le palier se dimensionne :
  `min` (toujours disponible), `soft` (cible), `hard` (plafond) et les
  octets `pinned` courants.
- `compress` — la voie de stockage (écriture) : `attempts` offerts,
  `accepted` et stockés, et le **taux d'acceptation** (acceptés /
  tentatives) — le taux de succès propre à ce palier pour la compression.
  En dessous, la ventilation des rejets : incompressible, politique,
  plafond, inéligible, réserve, part de tâche, et refus par emballement.
- `restore` — la voie de récupération (lecture) : `faults` de page,
  restaurations `warm`, restaurations `clustered` et leur total
  `restored` ; puis les `failures` (authentification / décodage) et le
  **taux de réussite** (restaurés / (restaurés + échecs)). Chaque taux est
  un pourcentage, ou `-` pour un dénominateur inactif.
- `warm-up` — les `attempts` du restaurateur à chaud en arrière-plan, son
  décompte `stopped` et son décompte `thrash-detected`.

### disks — stockage des volumes montés

Une ligne de style `df` par volume monté : point de montage, type de
système de fichiers, taille totale, utilisé, disponible, pourcentage
d'utilisation et une barre d'utilisation ASCII. Un volume dont le pilote
ne rapporte aucune capacité affiche `capacity unknown` au lieu d'une
taille fabriquée ; un volume retiré par surprise ou en conflit de
récupération est dessiné dans le rendu d'avertissement et marqué
(`[unavailable-dirty]`, `[unavailable-lost]`, `[recovery-conflict]`). Il
n'y a pas de compteurs de débit d'e-s par périphérique dans l'API, donc
ce sont capacité et utilisation honnêtes, pas des débits de transfert
fabriqués.

### cpu — charge par CPU

Une ligne par CPU : sa part occupée sur l'intervalle (`busy%`), la
profondeur de sa file d'exécution (`queue`) et ses décomptes de
changements de contexte (`switches`) et de préemptions (`preemptions`)
depuis le démarrage.

### irqs — lignes d'interruption

Une ligne par ligne d'interruption liée, en ordre croissant de ligne :
l'id de la ligne, la tâche pilote propriétaire (`owner`), le `count`
d'interruptions depuis le démarrage et le `state` de la ligne — `active`,
ou `quarantined` (dessiné dans le rendu d'avertissement) quand le filet de
sécurité du noyau contre les lignes emballées l'a désactivée.

### procs — le recensement des processus

Les plus gros consommateurs par `%cpu` et par mémoire (`size`), chacun
avec son pid, sa commande et — pour la table mémoire — son état. La liste
interactive complète des processus est le travail de `top` ; ceci n'est
que le résumé du recensement.

### Capacités

Chaque chiffre voyage par l'API d'information système. Les requêtes de
statistiques du noyau (mémoire, pression, caches, `ramzip`, charge par
CPU) exigent `CAP_SYSINFO_KERNEL` ; le panneau des lignes d'interruption
exige `CAP_SYSINFO_HW` ; le recensement de tous les processus exige
`CAP_SYSINFO_GLOBAL`. Un appelant qui manque de l'une voit le refus de ce
panneau explicité — jamais un chiffre fabriqué — pendant que le reste de
la session continue (échouer fermé, se dégrader avec grâce). Le stockage
des volumes montés n'est pas restreint.

## OPTIONS

- `-d, --delay <seconds>` — l'intervalle entre rafraîchissements
  automatiques, en secondes avec une fraction facultative (seul le premier
  chiffre décimal, les dixièmes, est conservé) : `sysmon -d 1.5`
  rafraîchit toutes les 1,5 secondes. Par défaut 3,0. GNU `top` accepte un
  intervalle nul et rafraîchit aussi vite qu'il le peut ; TAIRiX ne tourne
  jamais à vide, donc un zéro est relevé au minimum de 0,1 s.
- `-h, -?` — afficher l'aide brève de cette commande et quitter. Dans une
  session en cours, les mêmes touches basculent le pense-bête des touches.

## EXIT STATUS

- `0` — la session s'est terminée par `q`, ou l'aide brève a été affichée.
- `1` — le terminal a échoué ; la raison est écrite sur la sortie
  d'erreur.
- `2` — la ligne de commande n'a pas été comprise.

## ENVIRONMENT

- `LANG` — la locale préférée pour l'aide brève (une étiquette BCP-47
  telle que `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
