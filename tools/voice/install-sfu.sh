#!/usr/bin/env bash
set -Eeuo pipefail
IFS=$'\n\t'

REPOSITORY='https://github.com/crashjesuaquispeleon-bot/cinnabar-voichat.git'
BRANCH='feature/voice-chat-foundation'
SOURCE_DIR='/opt/cinnabar/cinnabar-voichat'
BINARY='/usr/local/bin/cinnabar-voice-sfu'
ENV_DIR='/etc/cinnabar'
ENV_FILE="${ENV_DIR}/voice-sfu.env"
TLS_DIR="${ENV_DIR}/tls"
SERVICE='cinnabar-voice-sfu'

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

[[ "${EUID}" -eq 0 ]] || fail 'Ejecuta este script con sudo: sudo bash install-sfu.sh'
[[ "$(uname -s)" == 'Linux' ]] || fail 'Este instalador requiere Linux.'
[[ "$(uname -m)" == 'aarch64' || "$(uname -m)" == 'arm64' ]] || fail 'Esta Alpha está preparada aquí para Ubuntu ARM64 (AArch64).'
command -v apt-get >/dev/null 2>&1 || fail 'Se requiere Ubuntu/Debian con apt-get.'

printf '\nCinnabar Voice SFU — instalador de prueba\n'
printf 'No instala el broker API y no crea una VM ni reglas de red en OCI.\n\n'

printf 'Instalando herramientas base...\n'
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y git ca-certificates curl openssl certbot golang-go python3 ufw iproute2

read -r -p 'Hostname nip.io (IPv4 pública en formato 1.2.3.4.nip.io): ' VOICE_HOST
[[ "$VOICE_HOST" == *.nip.io ]] || fail 'El hostname debe terminar en .nip.io.'
HOST_IP="${VOICE_HOST%.nip.io}"
python3 - "$HOST_IP" <<'PY' || fail 'La parte anterior a .nip.io debe ser una IPv4 pública válida.'
import ipaddress, sys
address = ipaddress.IPv4Address(sys.argv[1])
if address.is_private or address.is_loopback or address.is_link_local or address.is_reserved or address.is_multicast:
    raise SystemExit(1)
PY
getent ahostsv4 "$VOICE_HOST" | awk '{print $1}' | grep -Fxq "$HOST_IP" || fail 'nip.io todavía no resuelve a la IPv4 indicada. Comprueba la IP pública y el hostname.'
GLOBAL_IPV6="$(ip -6 -o addr show scope global | awk '{print $4}' | cut -d/ -f1 | python3 -c 'import ipaddress, sys; print(next((a for a in sys.stdin.read().split() if ipaddress.IPv6Address(a).is_global), "missing"))')"
[[ "$GLOBAL_IPV6" != "missing" ]] || fail 'La SFU requiere una IPv6 global en la VNIC; habilita IPv6 en la subnet de OCI y asígnala a la instancia.'

read -r -p 'Correo para avisos de renovación TLS: ' CERT_EMAIL
[[ "$CERT_EMAIL" == *@*.* ]] || fail 'Introduce un correo válido para Certbot.'
read -r -p 'Clave pública Ed25519 del broker (base64, 32 bytes): ' BROKER_PUBLIC_KEY
PUBLIC_KEY_BYTES="$(printf '%s' "$BROKER_PUBLIC_KEY" | base64 -d 2>/dev/null | wc -c | tr -d ' ')"
[[ "$PUBLIC_KEY_BYTES" == '32' ]] || fail 'La clave pública debe decodificar exactamente a 32 bytes.'
read -r -s -p 'Token administrativo SFU (32 caracteres hexadecimales; el mismo que guardarás en Cloudflare): ' ADMIN_TOKEN
printf '\n'
[[ "$ADMIN_TOKEN" =~ ^[a-fA-F0-9]{32}$ ]] || fail 'El token debe tener exactamente 32 caracteres hexadecimales.'
CONTROL_TOKEN_B64="$(printf '%s' "$ADMIN_TOKEN" | base64 -w0)"
unset ADMIN_TOKEN

printf '\nDependencias base instaladas.\n'

printf '\nClonando/actualizando el repositorio y emitiendo el certificado TLS...\n'
if [[ ! -d "$SOURCE_DIR/.git" ]]; then
    install -d -o root -g root /opt/cinnabar
    git clone --depth 1 --branch "$BRANCH" "$REPOSITORY" "$SOURCE_DIR"
else
    git -C "$SOURCE_DIR" fetch --depth 1 origin "$BRANCH"
    git -C "$SOURCE_DIR" checkout --detach FETCH_HEAD
fi

if [[ ! -f "/etc/letsencrypt/live/${VOICE_HOST}/fullchain.pem" || ! -f "/etc/letsencrypt/live/${VOICE_HOST}/privkey.pem" ]]; then
    certbot certonly --standalone --non-interactive --agree-tos \
        --email "$CERT_EMAIL" -d "$VOICE_HOST"
fi

printf '\nEjecutando pruebas Go y compilando el SFU...\n'
cd "$SOURCE_DIR/core"
GOTOOLCHAIN=auto go version
GOTOOLCHAIN=auto go test ./voice ./cmd/voice-sfu
GOTOOLCHAIN=auto go build -trimpath -ldflags='-s -w' -o /tmp/cinnabar-voice-sfu ./cmd/voice-sfu
install -o root -g root -m 0755 /tmp/cinnabar-voice-sfu "$BINARY"
rm -f /tmp/cinnabar-voice-sfu

if ! getent group cinnabar-voice >/dev/null; then
    groupadd --system cinnabar-voice
fi
if ! id cinnabar-voice >/dev/null 2>&1; then
    useradd --system --gid cinnabar-voice --no-create-home --shell /usr/sbin/nologin cinnabar-voice
fi

install -d -o root -g root -m 0711 "$ENV_DIR"
install -d -o root -g cinnabar-voice -m 0750 "$TLS_DIR"
install -o root -g cinnabar-voice -m 0640 "/etc/letsencrypt/live/${VOICE_HOST}/fullchain.pem" "${TLS_DIR}/fullchain.pem"
install -o root -g cinnabar-voice -m 0640 "/etc/letsencrypt/live/${VOICE_HOST}/privkey.pem" "${TLS_DIR}/privkey.pem"

umask 077
cat > "$ENV_FILE" <<ENV
CINNABAR_VOICE_LISTEN_ADDR=0.0.0.0:8443
CINNABAR_VOICE_TLS_CERT_FILE=${TLS_DIR}/fullchain.pem
CINNABAR_VOICE_TLS_KEY_FILE=${TLS_DIR}/privkey.pem
CINNABAR_VOICE_ISSUER=https://api.cinnabar.gg/voice
CINNABAR_VOICE_AUDIENCE=cinnabar-voice-sfu
CINNABAR_VOICE_BROKER_PUBLIC_KEY=${BROKER_PUBLIC_KEY}
CINNABAR_VOICE_CONTROL_TOKEN=${CONTROL_TOKEN_B64}
CINNABAR_VOICE_ICE_SERVERS=[{"urls":["stun:stun.l.google.com:19302"]}]
CINNABAR_VOICE_MAX_PARTICIPANTS=8
CINNABAR_VOICE_UDP4_PORT=19132
CINNABAR_VOICE_UDP6_PORT=19133
ENV
chown root:root "$ENV_FILE"
chmod 0600 "$ENV_FILE"
unset BROKER_PUBLIC_KEY CONTROL_TOKEN_B64 PUBLIC_KEY_BYTES

cat > "/etc/systemd/system/${SERVICE}.service" <<'UNIT'
[Unit]
Description=Cinnabar Voice SFU
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=cinnabar-voice
EnvironmentFile=/etc/cinnabar/voice-sfu.env
ExecStart=/usr/local/bin/cinnabar-voice-sfu
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true

[Install]
WantedBy=multi-user.target
UNIT

install -d -o root -g root -m 0755 /etc/letsencrypt/renewal-hooks/deploy
cat > /etc/letsencrypt/renewal-hooks/deploy/reload-cinnabar-voice-sfu <<HOOK
#!/bin/sh
set -eu
CERT=/etc/letsencrypt/live/${VOICE_HOST}
install -o root -g cinnabar-voice -m 0640 "\$CERT/fullchain.pem" /etc/cinnabar/tls/fullchain.pem
install -o root -g cinnabar-voice -m 0640 "\$CERT/privkey.pem" /etc/cinnabar/tls/privkey.pem
systemctl restart cinnabar-voice-sfu.service
HOOK
chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/reload-cinnabar-voice-sfu

if ufw status 2>/dev/null | grep -q '^Status: active'; then
    ufw allow 80/tcp
    ufw allow 8443/tcp
    ufw allow 19132/udp
    ufw allow 19133/udp
fi

systemctl daemon-reload
systemctl enable --now "${SERVICE}.service"
sleep 2
systemctl --no-pager --full status "${SERVICE}.service" || true

printf '\nInstalación del SFU terminada.\n'
printf 'Hostname: %s\n' "$VOICE_HOST"
printf 'WSS: wss://%s:8443/v1/voice\n' "$VOICE_HOST"
printf 'Health: https://%s:8443/healthz (debe responder HTTP 204)\n' "$VOICE_HOST"
printf '\nAbre en el NSG/Security List de OCI: TCP 80, TCP 8443, UDP 19132 y UDP 19133.\n'
printf 'SSH TCP 22 debe quedar limitado a tu IP de administración. No abras SSH a todo Internet.\n'
printf '\nEn Cloudflare debes desplegar cinnabar-api PR #1 y guardar el mismo token administrativo como secreto VOICE_SFU_ADMIN_TOKEN, la URL anterior como VOICE_SFU_ADMIN_URL y la clave privada Ed25519 como VOICE_ADMISSION_PRIVATE_KEY.\n'
printf 'En GitHub Actions Variables configura CINNABAR_VOICE_WSS_ENDPOINT y CINNABAR_VOICE_ICE_SERVERS con la URL WSS y el STUN público. Nunca guardes TURN credentials, clave privada ni token administrativo en Variables.\n'
printf '\nTodavía falta desplegar el Worker broker y verificar /healthz desde fuera antes de probar dos clientes.\n'
