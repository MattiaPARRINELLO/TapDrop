#!/usr/bin/env bash
# Relay for TapDrop: the laptop keeps a reverse SSH tunnel to 127.0.0.1:47913 on this server,
# and socat publishes it. No app needed on the phone, and the traffic stays end-to-end encrypted.
set -e
[ "$(id -u)" -eq 0 ] || { echo "à lancer en root" >&2; exit 1; }

command -v socat >/dev/null || {
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq socat
}

cat > /etc/systemd/system/tapdrop-relay.service <<'EOF'
[Unit]
Description=TapDrop relay (publishes the laptop's reverse SSH tunnel)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/bin/socat -d TCP-LISTEN:47913,fork,reuseaddr TCP:127.0.0.1:47914
Restart=always
RestartSec=3
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now tapdrop-relay.service

if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw limit 47913/tcp >/dev/null
    ufw status | grep 47913 || true
fi

sleep 1
systemctl is-active tapdrop-relay.service
ss -ltnp | grep 47913 || echo "rien n'écoute encore (le tunnel du PC n'est pas monté)"
