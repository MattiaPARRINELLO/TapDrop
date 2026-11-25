#!/usr/bin/env bash
# WireGuard hub for TapDrop. Run once on the server, as root.
#
# The hub does not carry internet traffic: phones and the PC only reach each other through
# 10.66.0.0/24. Each device keeps its own route to the internet.
set -euo pipefail

WG_DIR=/etc/wireguard
PORT=${WG_PORT:-51820}
SUBNET=${WG_SUBNET:-10.66.0}
IFACE=${WG_IFACE:-wg0}

[ "$(id -u)" -eq 0 ] || { echo "à lancer en root" >&2; exit 1; }

echo "→ paquets"
if command -v apt-get >/dev/null; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq wireguard qrencode
elif command -v dnf >/dev/null; then
    dnf install -y -q wireguard-tools qrencode
elif command -v pacman >/dev/null; then
    pacman -Sy --noconfirm --needed wireguard-tools qrencode
fi

umask 077
mkdir -p "$WG_DIR"

if [ ! -f "$WG_DIR/server.key" ]; then
    wg genkey > "$WG_DIR/server.key"
fi
wg pubkey < "$WG_DIR/server.key" > "$WG_DIR/server.pub"

if [ ! -f "$WG_DIR/$IFACE.conf" ]; then
    echo "→ configuration du concentrateur sur $SUBNET.1/24, port UDP $PORT"
    cat > "$WG_DIR/$IFACE.conf" <<EOF
[Interface]
Address = $SUBNET.1/24
ListenPort = $PORT
PrivateKey = $(cat "$WG_DIR/server.key")
PostUp = sysctl -q -w net.ipv4.ip_forward=1; iptables -C FORWARD -i $IFACE -j ACCEPT 2>/dev/null || iptables -I FORWARD -i $IFACE -j ACCEPT; iptables -C FORWARD -o $IFACE -j ACCEPT 2>/dev/null || iptables -I FORWARD -o $IFACE -j ACCEPT
PostDown = iptables -D FORWARD -i $IFACE -j ACCEPT 2>/dev/null || true; iptables -D FORWARD -o $IFACE -j ACCEPT 2>/dev/null || true
EOF
fi

echo "→ pare-feu de la machine"
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q active; then
    ufw allow "$PORT"/udp >/dev/null
elif command -v firewall-cmd >/dev/null && systemctl is-active --quiet firewalld; then
    firewall-cmd --permanent --add-port="$PORT"/udp >/dev/null
    firewall-cmd --reload >/dev/null
else
    # Oracle Cloud images ship an iptables ruleset that rejects most inbound traffic.
    iptables -C INPUT -p udp --dport "$PORT" -j ACCEPT 2>/dev/null \
        || iptables -I INPUT -p udp --dport "$PORT" -j ACCEPT
    if command -v netfilter-persistent >/dev/null; then
        netfilter-persistent save >/dev/null
    elif [ -d /etc/iptables ]; then
        iptables-save > /etc/iptables/rules.v4
    fi
fi

echo "→ service"
systemctl enable --now "wg-quick@$IFACE"

echo
echo "Concentrateur prêt."
echo "  clé publique : $(cat "$WG_DIR/server.pub")"
echo "  port         : $PORT/udp"
echo "  sous-réseau  : $SUBNET.0/24"
echo
echo "Pense à ouvrir $PORT/udp dans la liste de sécurité du cloud (console Oracle)."
echo "Ensuite : scripts/wireguard-peer.sh <nom> pour chaque appareil."
