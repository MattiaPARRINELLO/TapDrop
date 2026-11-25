#!/usr/bin/env bash
# Adds one device to the hub and prints its config, plus a QR code for the WireGuard app.
# Run on the server, as root: scripts/wireguard-peer.sh phone
set -euo pipefail

NAME=${1:?usage : wireguard-peer.sh <nom-de-l-appareil>}
WG_DIR=/etc/wireguard
IFACE=${WG_IFACE:-wg0}
PORT=${WG_PORT:-51820}
SUBNET=${WG_SUBNET:-10.66.0}
CONF=$WG_DIR/$IFACE.conf
OUT=${WG_OUT:-/root}

[ "$(id -u)" -eq 0 ] || { echo "à lancer en root" >&2; exit 1; }
[ -f "$CONF" ] || { echo "lance d'abord scripts/wireguard-hub.sh" >&2; exit 1; }

ENDPOINT=${WG_ENDPOINT:-$(curl -fsS4 https://api.ipify.org || curl -fsS4 https://ifconfig.me || true)}
[ -n "$ENDPOINT" ] || { echo "adresse publique introuvable, passe WG_ENDPOINT=hôte" >&2; exit 1; }

umask 077
last=$(grep -oE "AllowedIPs = $SUBNET\.[0-9]+" "$CONF" | grep -oE "[0-9]+$" | sort -n | tail -1 || true)
next=$(( ${last:-1} + 1 ))
[ "$next" -le 254 ] || { echo "le sous-réseau est plein" >&2; exit 1; }
PEER_IP=$SUBNET.$next

PRIVATE=$(wg genkey)
PUBLIC=$(printf '%s' "$PRIVATE" | wg pubkey)
SHARED=$(wg genpsk)

cat >> "$CONF" <<EOF

[Peer]
# $NAME
PublicKey = $PUBLIC
PresharedKey = $SHARED
AllowedIPs = $PEER_IP/32
EOF

wg syncconf "$IFACE" <(wg-quick strip "$IFACE")

CLIENT=$OUT/$NAME.conf
cat > "$CLIENT" <<EOF
[Interface]
PrivateKey = $PRIVATE
Address = $PEER_IP/24

[Peer]
PublicKey = $(cat "$WG_DIR/server.pub")
PresharedKey = $SHARED
Endpoint = $ENDPOINT:$PORT
AllowedIPs = $SUBNET.0/24
PersistentKeepalive = 25
EOF

echo
echo "$NAME est dans le réseau à $PEER_IP"
echo "configuration : $CLIENT"
echo "son adresse TapDrop sur ce réseau : $PEER_IP:47913"
echo
if command -v qrencode >/dev/null; then
    echo "QR code pour l'application WireGuard du téléphone :"
    qrencode -t ansiutf8 < "$CLIENT"
fi
