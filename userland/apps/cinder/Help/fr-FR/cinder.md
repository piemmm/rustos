## NAME

cinder — un compagnon de bureau qui vit dans un enclos et parcourt le bureau

## SYNOPSIS

`cinder`

## DESCRIPTION

Ouvre une petite fenêtre d'enclos avec Cinder dedans. Cinder est la mascotte de
TAIRiX : une petite créature rouille et anthracite qui flâne, fait sa toilette,
somnole et remarque où se trouve votre pointeur.

On peut le laisser sortir. Choisissez **Laisser sortir Cinder** dans son menu de
la barre d'icônes et il quitte l'enclos pour le bureau lui-même, où il se promène
entre vos fenêtres. Rencontrant une fenêtre, il grimpera dessus et s'assiéra sur
sa barre de titre, s'aplatira pour se glisser dessous, ou en fera simplement le
tour — selon la forme de la fenêtre et son humeur. Approchez le pointeur et il
le regardera, le suivra au trot, et bondira dessus s'il le rattrape.

Cliquez sur lui pour le caresser, dans l'enclos comme sur le bureau. Les
caresses lui remontent le moral. Sur le bureau, lui seul reçoit le clic :
l'espace transparent autour de lui appartient à ce qui se trouve derrière, si
bien qu'un compagnon posé sur votre travail n'avale jamais un clic qui lui
était destiné.

Dans l'enclos, vous pouvez aussi le prendre et le reposer n'importe où sur le
sol, et faire rouler la balle.

**Le menu.** **Laisser sortir Cinder** / **Faire rentrer Cinder** change l'endroit où il se
trouve.

**Quitter** y met fin et le retire du bureau.

**Fermer l'enclos.** Fermer la fenêtre de l'enclos ne quitte pas l'application. Cinder est une
application résidente : fermer l'enclos alors qu'il est dedans le range, et le
fermer alors qu'il est dehors le laisse vagabonder. Cliquez sur son icône dans
la barre pour rouvrir l'enclos. C'est *Quitter* qui y met fin.

**Humeur.** Cinder a trois besoins : le repos, le jeu et la compagnie. Courir dépense son
énergie et une sieste la restaure ; rester immobile l'ennuie et poursuivre le
pointeur le divertit ; les caresses lui remontent le moral. Vous n'avez rien à
gérer — il n'y a rien à lui donner à manger et rien ne tourne mal si vous le
laissez tranquille. Cela existe pour que vous puissiez voir d'un coup d'œil dans
quelle humeur il est.

Son humeur, et le fait qu'il était dehors, sont conservés d'une session à
l'autre.

**Quand il ne peut pas sortir.** Être sur le bureau hors d'une fenêtre requiert la capacité `CAP_DESKTOP_LAYER`,
que cette application demande dans son manifeste signé et que les droits de
votre compte doivent également autoriser. Sinon — ou s'il n'y a pas de session
graphique — **Laisser sortir Cinder** en indique la raison dans l'enclos et sur
la sortie d'erreur, et l'enclos continue de fonctionner. Cinder reste
simplement dedans.

## EXIT STATUS

`0` en cas de sortie propre. Un état non nul est toujours accompagné de la
raison sur la sortie d'erreur.

## SEE ALSO

`sapper`
