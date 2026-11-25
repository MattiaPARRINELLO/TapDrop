# Comment TapDrop a été porté sur Linux

Ce document raconte ce qui a été fait, mesuré et décidé pour faire tourner la moitié « ordinateur » de
TapDrop sur un PC Linux, avec l'application Android d'origine. Il sert de journal technique : les
chiffres viennent de mesures réelles sur la machine, pas d'estimations.

## 1. Le point de départ

TapDrop est publié sous MIT, avec un protocole documenté dans `PROTOCOL.md` : appairage P-256 avec code
à six chiffres, sessions chiffrées AES-256-GCM, lots de fichiers et de textes, lien live, et une preuve
de contact physique appelée *light key*. L'application Mac est en Swift, l'application Android en
Kotlin. Rien dans le protocole n'est spécifique à macOS : un hôte Linux peut donc se faire passer pour
le Mac, sans toucher à l'application du téléphone.

L'hôte Linux a été écrit en Rust, dans `linux/` : un seul binaire, un service utilisateur, une icône de
barre d'état, une fenêtre GTK, et une pastille animée.

## 2. La machine et ses capteurs

Machine : Lenovo Yoga 7 2-in-1 16ILL10, Garuda Linux, KDE Wayland, écran 120 Hz, facteur d'échelle 1,65.

Le module caméra (Luxvisions, USB) contient un hub de capteurs HID qui expose trois appareils IIO :

| Capteur | Chemin | Ce qu'il donne |
|---|---|---|
| lumière ambiante | `iio:device0` (`als`) | lux, échelle 0,1 |
| proximité | `iio:device1` | un bit proche/loin, qui n'a pas réagi au téléphone |
| distance | `iio:device2` | millimètres, 20 Hz |

### Mesures de lumière

| Situation | Capteur de lumière | Webcam (moyenne 0-255) |
|---|---|---|
| pièce éclairée | ~400 lux | 48 |
| écran blanc du téléphone collé | 1 057 lux | 173 |
| écran noir collé | 15 à 44 lux | 2,5 |

Le contraste est excellent dans les deux cas. Mais le capteur de lumière **ne rapporte qu'une mesure par
seconde** : les changements de valeur sont espacés de 1 009 à 1 015 ms, et les paliers durent des
multiples de 1 s. Forcer `sampling_frequency` à 100 Hz est accepté par le pilote mais le hub ignore la
demande. La caméra, elle, lit **30 images par seconde**, et la bascule se voit en une image.

### Mesures de proximité

Poser le téléphone fait passer la distance à `10000` (aucun retour) et le canal binaire de 1 à 0, en
40 à 100 ms. Une main produit exactement la même chose : la proximité ne peut donc **pas** prouver que
c'est un écran, seulement ouvrir la question.

## 3. Le protocole

Réimplémenté à l'identique : trames `u32` big-endian, JSON avant le chiffrement, `AES-256-GCM` avec
nonce `direction || compteur`, P-256 ECDH, HKDF-SHA256, code d'appairage `u32 % 1e6`.

Points vérifiés par les tests : HKDF conforme au vecteur RFC 5869, mêmes secrets des deux côtés, nonce
dépendant des deux nonces, rejeu refusé (le test rejoue une trame scellée et attend l'échec), aller et
retour d'un canal chiffré sur un duplex.

Côté fonctionnel : appairage, sessions `ping`, `send`, `getclip`, `inbox`, `watch`, `names`, `unpair`,
et le message chiffré `forgotten` pour un téléphone oublié.

## 4. Le tap

Le protocole prévoit une « light key » : le téléphone éclaire son écran en blanc, puis en noir, et
l'hôte doit voir la lumière puis l'obscurité. La version Mac utilise le capteur du notch, qui échantillonne
vite. Sur ce PC, à une mesure par seconde, les fenêtres de 450 ms du protocole sont impossibles.

Décision : **la proximité déclenche, la lumière prouve**.

1. la proximité voit le contact, le round part en quelques dizaines de ms ;
2. la caméra guette l'écran blanc, le seuil vaut 45 % de la luminosité de contact apprise ;
3. l'écran noir doit faire chuter la mesure, et la bosse doit être plus sombre que la pièce.

Un round complet prend 240 à 280 ms, et le seuil s'apprend par la médiane des douze derniers taps
réussis (1 042 lux mesurés ici, au lieu des 300 par défaut).

## 5. La caméra comme capteur

L'ambiant light sensor à 1 Hz ratait les éclairs. La webcam, dans la même bosse, lit 30 images par
seconde : elle a remplacé le capteur pour le tap, avec trois précautions.

- **exposition fixe** (`auto_exposure=1`, temps 80) : l'exposition automatique ramènerait un écran blanc
  à un gris moyen, ce qui détruit la mesure ;
- **exposition restaurée** après chaque round, pour ne pas casser les visioconférences ;
- **la caméra ne s'ouvre que pendant un round** : la LED visible est le coût, environ un quart de seconde.

La moyenne de luminance est ramenée dans les mêmes unités que le capteur de lumière (`×8`), ce qui
permet de réutiliser tel quel l'apprentissage du seuil et la vérification de l'écho sombre.

La caméra infrarouge a été écartée : un écran n'émet pas d'infrarouge, elle ne verrait que son propre
illuminateur, donc elle se comporte comme la proximité.

## 6. L'îlot

L'îlot est une surface Wayland de type layer (`gtk4-layer-shell`), ancrée en haut et centrée,
transparente aux clics sauf sur la pastille. Il dessine avec Cairo et anime avec de vrais ressorts
intégrés image par image, pilotés par le frame clock (120 Hz sur cette machine).

Choix de conception :

- géométrie ouverte depuis le bord haut, comme si elle sortait de la barre caméra ;
- largeur mesurée sur le texte (Pango), bornée à 320 et 520 px ;
- ressorts à vitesse conservée : un changement d'état retarget le ressort au lieu de repartir de zéro ;
- entrée chorégraphiée (icône, titre, détail, barre, décalés de 30 ms) ;
- coche qui se trace, anneau vert qui s'ouvre ;
- halo teinté par l'état, liseré lumineux en haut, dégradé qui dérive sur 10 s ;
- mises en page Pango en cache, pour ne rien recalculer à 120 images par seconde.

Deux bogues ont été trouvés et corrigés en route, tous les deux visibles à l'usage :

1. la surface gardait sa dernière image quand l'îlot se cachait, le compositeur n'effaçant rien : la
   fenêtre est maintenant **démontée** quand il n'y a rien à montrer ;
2. les événements partaient de sockets différents et pouvaient arriver dans le désordre, un « réception »
   après un « Enregistré » laissant la carte bloquée : un socket unique garantit l'ordre.

## 7. Barre d'état et fenêtre

L'icône est un StatusNotifierItem (`ksni`) avec menu : ouvrir, envoyer le presse-papiers, accepter ou
refuser un appairage, quitter. La fenêtre est en GTK4 et parle au service par le socket de contrôle, ce
qui permet de la fermer sans arrêter le service.

Le service tourne en `systemd --user`, avec `Restart=always` et démarrage à la session.

## 8. Le réseau, et l'isolation du Wi-Fi d'école

Sur un réseau normal, la découverte DNS-SD suffit. Sur le Wi-Fi de l'école, l'isolation des clients
empêche deux appareils de se joindre, même s'ils se voient en mDNS. Quatre contournements ont été mis
en place, dans cet ordre de simplicité :

1. **USB** : `adb reverse tcp:47913 tcp:47913`, puis `127.0.0.1:47913` dans l'application ;
2. **partage de connexion** du téléphone, où la découverte remarche ;
3. **relais SSH** : le PC ouvre une connexion sortante vers le serveur et y publie le port, `socat`
   l'expose ; le téléphone se connecte à l'adresse du serveur, sans rien installer ;
4. **concentrateur WireGuard** sur le serveur, avec un sous-réseau privé `10.66.0.0/24` et un QR code
   pour le téléphone.

Le relais SSH a été retenu : aucune application à installer sur le téléphone, et le contenu reste
chiffré de bout en bout, le serveur ne voyant que du chiffrement TapDrop.

## 9. Décisions et compromis

| Décision | Raison |
|---|---|
| proximité comme déclencheur | seule source rapide et fiable au contact |
| lumière comme preuve | une main ne peut pas éclairer un écran |
| caméra plutôt que capteur de lumière | 30 Hz contre 1 Hz |
| exposition fixe | l'exposition automatique annule le contraste |
| appairage automatique désactivé en public | le port du relais est exposé |
| socket unique vers l'îlot | l'ordre des événements compte |
| surface démontée au repos | aucune image fantôme possible |
| largeur au contenu | plus de troncature, mouvement plus juste |

## 10. Ce qui reste

- reconstruire l'APK avec le nettoyage d'URL et des textes « PC » plutôt que « Mac » ;
- Samsung Internet n'expose que le domaine : rien à faire côté hôte, le partage reste la solution
  exacte pour ce navigateur ;
- vrai flou de verre impossible pour l'instant : l'effet KWin maintenu s'arrête à Plasma 6.4, cette
  machine est en 6.8.3. Le protocole `org_kde_kwin_blur` reste une piste, au prix d'une intégration
  Wayland fragile ;
- icône et textes de l'application Android dans la langue de l'utilisateur.

## 11. Chiffres retenus

```
capteur de lumière : ~1 mesure / s, pièce 400 lx, écran blanc 1057 lx, écran noir 15-44 lx
webcam             : 30 images / s, pièce 48, écran blanc 173, écran noir 2,5, exposition 80
proximité          : 20 Hz, réaction 40-100 ms, 10000 = aucun retour
tap complet        : 240-280 ms
seuil appris       : 1 042 lux (0,45 × niveau de contact)
affichage          : 120 Hz, échelle 1,65
```
