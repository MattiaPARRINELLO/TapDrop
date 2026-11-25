# TapDrop pour Linux

L'hôte Linux de [TapDrop](../../README.md) : le téléphone Android envoie, l'ordinateur reçoit, sans
câble, sur le réseau local ou à travers ton propre serveur.

Ce dossier contient la moitié « ordinateur » du projet, écrite en Rust. L'application Android d'origine
fonctionne telle quelle : ce programme parle exactement le protocole décrit dans le
[`PROTOCOL.md`](../../PROTOCOL.md) du dépôt.

## Ce que ça fait

| Fonction | Détail |
|---|---|
| Appairage | code à six chiffres, P-256 ECDH, HKDF-SHA256, approbation sur le PC |
| Sessions | AES-256-GCM, compteurs implicites, rejeu et réordonnancement refusés |
| Réception | fichiers, photos, vidéos, textes, codes à usage unique, images pour le presse-papiers |
| Presse-papiers | dans les deux sens, texte, image ou fichiers copiés, via `wl-clipboard` |
| Liens | ouverts dans le navigateur, avec nettoyage des URL abîmées |
| OCR | lit le texte des captures d'écran avec `tesseract` |
| File d'envoi | `tapdrop-linux send`, annoncée sur le lien live du téléphone |
| Découverte | DNS-SD `_tapdrop._tcp` via avahi, ou répondeur mDNS intégré |
| Tap | le téléphone posé sur la bosse caméra envoie ce qui est à l'écran |
| Îlot | pastille animée en haut de l'écran, alignée sur le design system du projet : Geist, palette night/bone, anneau de progression, vignette du fichier reçu, boutons Ouvrir et Dossier, ressorts d'Apple |
| Barre d'état | icône StatusNotifierItem avec menu, appairages en cours, quitter |
| Fenêtre | état, téléphones, appairage, réglages (`tapdrop-linux ui`) |

## Dépendances

Arch :

```sh
sudo pacman -S --needed rust gtk4 gtk4-layer-shell wl-clipboard avahi tesseract tesseract-data-fra \
    libnotify wireguard-tools socat
```

Ubuntu 24.04 :

```sh
sudo apt install build-essential libgtk-4-dev libgtk4-layer-shell-dev wl-clipboard avahi-daemon \
    tesseract-ocr tesseract-ocr-fra libnotify-bin wireguard-tools socat
```

`tesseract`, `avahi` et `gtk4-layer-shell` sont facultatifs : sans eux, l'OCR tombe sur le presse-papiers
image, la découverte passe par le répondeur intégré, et l'îlot ne peut pas s'afficher.

## Compiler et lancer

```sh
cargo build --release
./target/release/tapdrop-linux run
```

Le service écoute sur le port **47913** (un port libre s'il est pris) et s'annonce sous le nom de la
machine. Sur le téléphone : TapDrop, **Pair with your Mac**, choisir l'ordinateur, comparer le code,
puis l'approuver :

```sh
tapdrop-linux pending
tapdrop-linux approve 123456
```

## Service utilisateur

Le service tourne sans session ouverte et redémarre seul. Installer le binaire dans le `PATH`, puis
l'unité :

```sh
install -Dm755 target/release/tapdrop-linux ~/.local/bin/tapdrop-linux
mkdir -p ~/.config/systemd/user
cp scripts/tapdrop.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tapdrop
journalctl --user -u tapdrop -f
```

## Commandes

```
tapdrop-linux run [--auto-pair]   démarrer le service (avec son icône)
tapdrop-linux ui                  ouvrir la fenêtre
tapdrop-linux island              afficher l'îlot seul (débogage)
tapdrop-linux status              état, appareils, adresses joignables
tapdrop-linux sensors             ce que voient la lumière et la proximité
tapdrop-linux send <fichier…>     mettre des fichiers en file
tapdrop-linux send --clipboard    envoyer le presse-papiers
tapdrop-linux devices             téléphones appairés
tapdrop-linux forget <id>         oublier un téléphone
tapdrop-linux rename <nom>        renommer cet ordinateur
tapdrop-linux folder <chemin>     changer le dossier de réception
tapdrop-linux pending | approve <code> | deny <code>
tapdrop-linux quit                arrêter le service
```

## Fichiers et sockets

| Quoi | Où |
|---|---|
| état, appareils, file | `~/.config/tapdrop-linux/state.toml` |
| socket de contrôle | `$XDG_RUNTIME_DIR/tapdrop-linux/control.sock` |
| socket de l'îlot | `$XDG_RUNTIME_DIR/tapdrop-linux/island.sock` |
| réception | `~/Téléchargements` (dossier XDG) par défaut |

## Comment un tap est jugé

Trois preuves, dans cet ordre :

1. **proximité** : quelque chose se pose sur la bosse caméra (20 Hz, quelques dizaines de ms) ;
2. **lumière** : le téléphone dit « écran blanc », la caméra doit voir la bosse s'éclairer franchement ;
3. **écho sombre** : le téléphone dit « écran noir », la caméra doit voir la bosse plus sombre que la pièce.

Une main peut produire 1 et 3, jamais 2. Une lampe peut produire 2, jamais 3. Sur cette machine, un tap
complet prend 240 à 400 ms, et le seuil de lumière s'apprend tout seul à partir des taps réussis.

Le capteur de luminosité du châssis ne rapporte qu'une mesure par seconde : il sert de secours quand la
webcam est occupée. La caméra est utilisée en exposition fixe, sinon l'exposition automatique ramènerait
l'écran blanc à un gris moyen.

## Hors du réseau local

TapDrop ne passe par aucun serveur par défaut. Quand les deux appareils ne peuvent pas se joindre
(Wi-Fi d'école avec isolation des clients, téléphone en 4G), quatre solutions, de la plus simple à la
plus souple :

1. **USB** : `adb reverse tcp:47913 tcp:47913`, puis `127.0.0.1:47913` dans TapDrop → *Edit address*.
2. **Partage de connexion** du téléphone (point d'accès ou Bluetooth) : le PC rejoint le réseau du
   téléphone, la découverte automatique remarche.
3. **Relais SSH par ton serveur** : le PC garde une connexion sortante et publie le port, le téléphone
   se connecte à l'adresse du serveur. Voir `scripts/relay-ssh.sh` et `scripts/tapdrop-tunnel.service`.
   Ouvrir TCP 47913 dans la security list du cloud. Aucune application à installer sur le téléphone.
4. **Concentrateur WireGuard** sur ton serveur : `scripts/wireguard-hub.sh` et `scripts/wireguard-peer.sh`
   produisent les configurations et un QR code pour l'application WireGuard.

## Architecture

| Module | Rôle |
|---|---|
| `wire.rs` | trames préfixées, canal AES-256-GCM à compteurs |
| `crypto.rs` | P-256 ECDH, HKDF-SHA256, code d'appairage |
| `server.rs` | écoute TCP, appairage, sessions, réception, envoi |
| `watch.rs` | lien live, présence, caps, renommage |
| `lightkey.rs` | machine à états d'un round (proximité, lumière, écho) |
| `tap.rs` | relie capteurs, caméra et lien live |
| `iio.rs` | capteur de lumière et de proximité |
| `camera.rs` | webcam en capteur de lumière, 30 images par seconde |
| `island.rs` | pastille animée : forme d'encoche, anneau, coche, ressorts, socket d'événements |
| `tray.rs` | icône StatusNotifierItem et son menu |
| `ui.rs` | fenêtre GTK4 |
| `mdns.rs` | découverte DNS-SD (avahi ou répondeur intégré) |
| `net.rs` | adresses joignables (Wi-Fi, WireGuard, Bluetooth) |
| `store.rs` | état persistant, appareils, file, noms |
| `files.rs` | noms sûrs, fichiers partiels, pas d'écrasement |
| `clipboard.rs` | presse-papiers Wayland |
| `control.rs` | socket de contrôle et client bloquant pour GTK |

## Design

L'îlot suit [`docs/DESIGN.md`](../docs/DESIGN.md) plutôt que d'improviser : nuit `#08080A`, texte bone
`#F4F3EF`, secondaire `#A2A2A8`, halo `#D6DEFF`, aucune teinte d'accent, géométrie d'encoche (haut plat,
bas arrondi), anneau de progression en visuel de tête, coche encre sur disque bone pour le succès, et les
ressorts du système (îlot 0,42 s / 0,78, contenu 0,38 s / 0,86).

La surface ignore la réserve du panneau (`set_exclusive_zone(-1)`), pour partir du bord physique de
l'écran comme une vraie encoche, au lieu d'être poussée sous la barre du haut.

Quand un fichier arrive, sa vignette est décodée hors du fil d'affichage (une photo de téléphone
bloquerait l'animation), dessinée en 26 px arrondis, et deux boutons discrets apparaissent à droite :
ouvrir le fichier, ou son dossier. Le résultat reste huit secondes à l'écran, et le compte à rebours
s'arrête tant que la souris est sur la pastille.

Les deux fontes du projet sont embarquées dans le binaire et enregistrées dans le processus au démarrage,
sans toucher au dossier de fontes de la personne : `fonts/Geist[wght].ttf` et `fonts/GeistMono[wght].ttf`,
les mêmes fichiers OFL que dans `fonts/` à la racine du dépôt.

## Limites connues

- **Samsung Internet** n'expose que le domaine dans l'arbre d'accessibilité, jamais le chemin. Un tap
  depuis Samsung ouvre donc le site, pas la page exacte. Chrome expose l'URL complète. Le partage
  depuis Samsung envoie l'URL exacte.
- **Caméra infrarouge** inutile ici : un écran n'émet pas d'infrarouge, la caméra IR ne verrait que son
  propre illuminateur, donc elle se comporte comme un détecteur de présence.
- **LED de la webcam** allumée pendant un tap, environ un quart de seconde.
- **Vrai flou** impossible pour l'instant : l'effet KWin disponible ne supporte pas Plasma 6.8.
- L'APK officiel affiche encore « Mac » : voir le correctif Android dans ce dépôt.

## Dépannage

```sh
tapdrop-linux sensors                 # lumière et proximité en direct
journalctl --user -u tapdrop -n 50    # journal du service
systemctl --user status tapdrop
tapdrop-linux status                  # adresses à saisir sur le téléphone
```

Un tap refusé affiche la consigne dans l'îlot (`Retourne le téléphone, écran vers la bosse`, `Colle
l'écran à plat sur la bosse`, `Garde le téléphone une seconde de plus`). Le journal donne le motif exact
du round : `facing`, `not-seen`, `stayed-lit`, `busy`, `cooldown`.

## Licence

MIT, comme TapDrop. Le protocole et les applications d'origine sont de
[0x1p0](https://github.com/0x1p0/TapDrop).
