# Despliegue de Cinnabar Voice SFU para la Alpha de dos jugadores

Este documento prepara un despliegue de prueba en una máquina Linux pública. **No despliega nada automáticamente** ni contiene secretos. El SFU y el servidor Minecraft Bedrock deben estar en hosts distintos para conservar los puertos Bedrock.

## Opción inicial sin VPS de pago: Oracle Cloud Always Free

La opción recomendada para intentar una prueba de bajo costo es una VM OCI Ampere A1 elegible para Always Free:

- Shape: VM.Standard.A1.Flex, 2 OCPU y 12 GB de RAM dentro del límite gratuito.
- Imagen: Ubuntu Server ARM64 (AArch64), versión LTS vigente.
- Red: subred pública, dirección IPv4 pública y una IPv6 pública en una VCN/subred habilitada para IPv6.
- Disco: dentro del almacenamiento Always Free.
- Región: la región principal de la cuenta. Always Free no está disponible en cualquier región secundaria.

La creación depende de capacidad en la región principal; un error de capacidad agotada no significa que la cuenta esté mal configurada. Oracle también puede recuperar instancias Always Free que cumplan sus criterios de inactividad durante siete días. Por ello sirve para una prueba Alpha, no debe tratarse como SLA ni como infraestructura de producción. La documentación técnica de Always Free indica un total de 2 OCPU y 12 GB para A1, sujeto a capacidad y cuota de cuenta.

**No puedo garantizar que el despliegue completo cueste S/ 0.** Antes de crear recursos comprueba en la consola el costo de la IPv4 pública, red, almacenamiento y cualquier otro elemento; continúa solo si cada recurso aparece como Always Free o el estimador muestra el costo que aceptas. La página de registro de Oracle indica que a la mayoría de usuarios se les pide teléfono y tarjeta para verificar la cuenta, y que no se cobra la tarjeta a menos que actualicen a una cuenta de pago.

Si no hay capacidad A1 en la región principal, no cambies a una máquina de pago sin revisar el costo. Una alternativa es un VPS económico con IPv4 e IPv6 públicas, pero ya no sería gratuito.

## Crear la VM en Oracle Cloud (paso a paso)

1. Abre [Oracle Cloud Free Tier](https://www.oracle.com/latam/cloud/free/) y crea una cuenta. Usa una sola cuenta personal, datos reales y un método de pago aceptado para la verificación. Oracle indica que puede hacer una retención temporal de autorización; revisa las condiciones antes de continuar.
2. En la consola, confirma tu **Home Region**. Los recursos Always Free A1 deben crearse en esa región. Si no hay capacidad A1, no elijas una forma de pago ni aumentes la cuota sin revisar el costo.
3. Abre **Networking → Virtual Cloud Networks → Start VCN Wizard** y elige **Create VCN with Internet Connectivity**. Habilita IPv6 durante el asistente. Conserva la subnet pública, Internet Gateway y ruta predeterminada hacia ese gateway.
4. Ve a **Compute → Instances → Create instance**. Nombre: `cinnabar-voice-sfu`. Imagen: Ubuntu Server ARM64/AArch64 LTS y comprueba la etiqueta **Always Free eligible**.
5. En Shape selecciona `VM.Standard.A1.Flex`. Para mantenernos dentro del límite técnico conservador, comienza con 2 OCPU y 12 GB de RAM como máximo. Oracle muestra actualmente límites distintos en su página comercial y en la referencia técnica de Always Free; manda la etiqueta y el consumo Always Free que aparezcan en tu consola, no aumentes recursos basándote solo en una página promocional.
6. En **Networking / Primary VNIC** selecciona la VCN y subnet pública que creaste. Marca la asignación automática de IPv4 pública. En **Assign IPv6 addresses from subnet prefixes**, asigna una dirección del prefijo IPv6 global. No basta una IPv6 ULA privada. Adjunta tu clave SSH pública.
7. Antes de crear la instancia, vuelve a revisar que el estimador indique solo recursos Always Free o un importe que aceptas. El volumen de arranque también consume la cuota de almacenamiento.
8. Cuando la VM esté en estado Running, copia su IPv4 pública. El hostname que usaremos será esa dirección seguida de `.nip.io`, por ejemplo `IP_PUBLICA.nip.io`. Conéctate con el usuario `ubuntu` y la clave SSH privada que guardaste localmente.

Oracle documenta el asistente de VCN, la elección de subred pública y la asignación de IPv4/IPv6 en sus [pasos para crear instancias](https://docs.oracle.com/en-us/iaas/Content/Compute/Tasks/launchinginstance.htm) y la [guía de IPv6](https://docs.oracle.com/en-us/iaas/Content/Network/Concepts/ipv6.htm). En una subnet pública también se necesita una regla de ruta al Internet Gateway y reglas de seguridad que permitan el tráfico.

### Reglas de entrada en OCI

Agrega estas reglas en el NSG de la VNIC o en la Security List de la subnet. Usa reglas stateful:

| Origen | Protocolo | Puerto de destino | Motivo |
| --- | --- | ---: | --- |
| Tu IPv4 pública con `/32` | TCP | 22 | Administración SSH |
| `0.0.0.0/0` | TCP | 80 | Certbot HTTP-01 y renovación TLS |
| `0.0.0.0/0` | TCP | 8443 | WSS del SFU y operaciones HTTPS del broker |
| `0.0.0.0/0` | UDP | 19132 | ICE/WebRTC IPv4 |
| `::/0` | UDP | 19133 | ICE/WebRTC IPv6 |

No abras SSH a `0.0.0.0/0` ni a `::/0`. Si la Security List tiene salida restringida, permite también DNS y HTTPS para Certbot/descarga de Go y el tráfico UDP saliente requerido para ICE/STUN. La SFU y el servidor Bedrock siguen siendo máquinas distintas.

## Red y DNS

No compraremos ni registraremos un dominio. Usaremos **nip.io**, DNS público que resuelve automáticamente un hostname que contiene la IPv4, por ejemplo `203.0.113.10.nip.io` (solo ejemplo reservado; no lo copies). Cuando Oracle asigne la IP pública real, sustituye `IP_PUBLICA` por sus cuatro octetos en `IP_PUBLICA.nip.io`. No hay que crear un registro DNS manual. El hostname resultante sirve para solicitar un certificado TLS público. Si la IP pública cambia, también cambia el hostname: habrá que emitir el certificado otra vez y recompilar las dos aplicaciones.

En la VCN/NSG o Security List de OCI y en el firewall Linux permite únicamente:

| Protocolo | Puerto | Uso |
| --- | ---: | --- |
| TCP | 22 | SSH; restringido a tu IP de administración |
| TCP | 80 | Validación HTTP de Let's Encrypt al emitir/renovar el certificado |
| TCP | 8443 | WSS y operaciones administrativas HTTPS del SFU |
| UDP | 19132 | ICE/WebRTC IPv4 |
| UDP | 19133 | ICE/WebRTC IPv6 |

La SFU actual abre sockets IPv4 e IPv6 por separado; por eso la VM debe tener una interfaz IPv6 habilitada para usar UDP 19133. No abras SSH a todo Internet. No configures puertos 19132/19133 en la máquina de Bedrock: pertenecen al host separado del SFU durante esta prueba.

## Instalación automática del SFU (recomendada)

Después de crear la VM, asignar una IPv4 pública, habilitar una IPv6 global y abrir las reglas de red de la tabla anterior, ejecuta en Ubuntu ARM64:

~~~sh
curl -fsSL https://raw.githubusercontent.com/crashjesuaquispeleon-bot/cinnabar-voichat/feature/voice-chat-foundation/tools/voice/install-sfu.sh -o install-sfu.sh
sudo bash install-sfu.sh
~~~

El instalador verifica que el nombre `IP_PUBLICA.nip.io` resuelva a la IPv4 indicada y que exista una IPv6 global, solicita el correo de Certbot, la clave **pública** Ed25519 del broker y el token administrativo (lo solicita sin mostrarlo), emite el certificado, compila y prueba el SFU, instala el servicio `systemd` y configura la renovación del certificado. Guarda el token administrativo para introducir exactamente el mismo valor como secreto `VOICE_SFU_ADMIN_TOKEN` en Cloudflare. El instalador no recibe ni instala la clave privada Ed25519.

El script no crea la cuenta Oracle, la VM, las reglas NSG, el broker Cloudflare ni las variables de compilación GitHub. Tampoco aprueba por sí solo la prueba real entre dos jugadores. Si falla la verificación de IPv6, vuelve a la VNIC y la subnet de OCI: el SFU actual no arranca solo con IPv4.

## Instalación manual alternativa del SFU en ARM64

Conéctate por SSH a Ubuntu y ejecuta:

~~~sh
sudo apt-get update
sudo apt-get install -y git ca-certificates openssl certbot ufw golang-go
sudo mkdir -p /opt/cinnabar
sudo git clone --depth 1 --branch feature/voice-chat-foundation \
  https://github.com/crashjesuaquispeleon-bot/cinnabar-voichat.git \
  /opt/cinnabar/cinnabar-voichat
cd /opt/cinnabar/cinnabar-voichat/core

# El go.mod fija Go 1.26.1; GOTOOLCHAIN=auto descarga esa toolchain si hace falta.
GOTOOLCHAIN=auto go version
GOTOOLCHAIN=auto go test ./voice ./cmd/voice-sfu
GOTOOLCHAIN=auto go build -trimpath -ldflags="-s -w" \
  -o /tmp/cinnabar-voice-sfu ./cmd/voice-sfu
sudo install -o root -g root -m 0755 /tmp/cinnabar-voice-sfu \
  /usr/local/bin/cinnabar-voice-sfu
rm -f /tmp/cinnabar-voice-sfu
~~~

No ejecutes el binario hasta crear el archivo de configuración, instalar el certificado y completar la configuración del broker.

## Certificado TLS

Apunta el registro A de tu nombre al IPv4 público de la VM y permite temporalmente TCP 80 para la validación HTTP:

~~~sh
sudo certbot certonly --standalone -d IP_PUBLICA.nip.io
~~~

Los archivos esperados son:

- `/etc/letsencrypt/live/IP_PUBLICA.nip.io/fullchain.pem`
- `/etc/letsencrypt/live/IP_PUBLICA.nip.io/privkey.pem`

El servicio SFU debe poder leer el certificado y la clave privada. Mantén sus permisos restrictivos. Configura el timer de renovación de Certbot y un deploy hook que reinicie `cinnabar-voice-sfu` después de renovar el certificado, ya que el proceso carga los archivos TLS al arrancar.

## Claves y secretos del broker

Genera los secretos localmente en un equipo de confianza. No los publiques en GitHub, capturas, chat ni logs.

1. Genera una clave Ed25519 en formato PKCS#8 para el Worker:

~~~sh
openssl genpkey -algorithm Ed25519 -out voice-broker-private.pem
openssl pkey -in voice-broker-private.pem -pubout -out voice-broker-public.pem
openssl pkey -in voice-broker-private.pem -pubout -outform DER | tail -c 32 | base64 -w0
~~~

El PEM privado se carga únicamente como secreto `VOICE_ADMISSION_PRIVATE_KEY` del Worker. El valor base64 de los 32 bytes públicos se configura como `CINNABAR_VOICE_BROKER_PUBLIC_KEY` en el SFU.

2. Genera un token administrativo de 32 caracteres ASCII imprimibles:

~~~sh
openssl rand -hex 16
~~~

Ese texto exacto se guarda como secreto `VOICE_SFU_ADMIN_TOKEN` del Worker. En el servidor SFU se configura `CINNABAR_VOICE_CONTROL_TOKEN` como Base64 de esos mismos 32 caracteres, no como la cadena hexadecimal sin codificar.

3. El issuer y audience deben coincidir exactamente entre broker y SFU:

- Issuer: `https://api.cinnabar.gg/voice`
- Audience: `cinnabar-voice-sfu`

4. El Worker también necesita `VOICE_SFU_ADMIN_URL=https://IP_PUBLICA.nip.io:8443`. Debe ser solo el origen HTTPS, sin `/v1/voice`, ruta ni query string.

La clave privada nunca se copia al SFU ni a los clientes. El SFU recibe solo la clave pública. El token administrativo jamás se distribuye a los clientes.

## Archivo de entorno del SFU

Crea `/etc/cinnabar/voice-sfu.env` con permisos 600 y propietario root. Sustituye todos los marcadores por valores reales obtenidos de forma segura:

~~~ini
CINNABAR_VOICE_LISTEN_ADDR=0.0.0.0:8443
CINNABAR_VOICE_TLS_CERT_FILE=/etc/cinnabar/tls/fullchain.pem
CINNABAR_VOICE_TLS_KEY_FILE=/etc/cinnabar/tls/privkey.pem
CINNABAR_VOICE_ISSUER=https://api.cinnabar.gg/voice
CINNABAR_VOICE_AUDIENCE=cinnabar-voice-sfu
CINNABAR_VOICE_BROKER_PUBLIC_KEY=BASE64_CLAVE_PUBLICA_ED25519
CINNABAR_VOICE_CONTROL_TOKEN=BASE64_DE_LOS_32_CARACTERES_DEL_TOKEN
CINNABAR_VOICE_ICE_SERVERS=[{"urls":["stun:stun.l.google.com:19302"]}]
CINNABAR_VOICE_MAX_PARTICIPANTS=8
CINNABAR_VOICE_UDP4_PORT=19132
CINNABAR_VOICE_UDP6_PORT=19133
~~~

Para esta Alpha usaremos el servidor STUN público que Google documenta en su herramienta WebRTC: `stun:stun.l.google.com:19302`. Es solo una ayuda para descubrir direcciones reflejadas; no garantiza conexión en redes con NAT simétrico o firewall restrictivo. No tiene SLA y no equivale a TURN. El flujo actual no emite credenciales TURN efímeras para jugadores; para redes que necesiten relay habrá que implementar ese suministro seguro en el broker antes de una publicación abierta.

## Servicio systemd

Crea el usuario de servicio y concede solo lectura de las copias TLS al grupo dedicado del servicio:

~~~sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin cinnabar-voice
# Parent is traversable but not listable; TLS files are readable only by this service group.
sudo install -d -o root -g root -m 0711 /etc/cinnabar
sudo install -d -o root -g cinnabar-voice -m 0750 /etc/cinnabar/tls
sudo install -o root -g cinnabar-voice -m 0640 \\
  /etc/letsencrypt/live/IP_PUBLICA.nip.io/fullchain.pem /etc/cinnabar/tls/fullchain.pem
sudo install -o root -g cinnabar-voice -m 0640 \\
  /etc/letsencrypt/live/IP_PUBLICA.nip.io/privkey.pem /etc/cinnabar/tls/privkey.pem
sudo chown root:root /etc/cinnabar/voice-sfu.env
sudo chmod 0600 /etc/cinnabar/voice-sfu.env
~~~

Guarda lo siguiente en `/etc/systemd/system/cinnabar-voice-sfu.service`:

~~~ini
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
~~~

Crea el hook de renovación para actualizar las copias protegidas y reiniciar el proceso que carga TLS al arrancar:

~~~sh
sudo install -d -o root -g root -m 0755 /etc/letsencrypt/renewal-hooks/deploy
sudo tee /etc/letsencrypt/renewal-hooks/deploy/reload-cinnabar-voice-sfu >/dev/null <<'EOF'
#!/bin/sh
set -eu
CERT=/etc/letsencrypt/live/IP_PUBLICA.nip.io
install -o root -g cinnabar-voice -m 0640 "$CERT/fullchain.pem" /etc/cinnabar/tls/fullchain.pem
install -o root -g cinnabar-voice -m 0640 "$CERT/privkey.pem" /etc/cinnabar/tls/privkey.pem
systemctl restart cinnabar-voice-sfu.service
EOF
sudo chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/reload-cinnabar-voice-sfu
~~~

Activa el servicio:

~~~sh
sudo systemctl daemon-reload
sudo systemctl enable --now cinnabar-voice-sfu
sudo systemctl status cinnabar-voice-sfu --no-pager
sudo journalctl -u cinnabar-voice-sfu -n 100 --no-pager
~~~

Para UFW, si está habilitado, replica las reglas permitidas en OCI:

~~~sh
sudo ufw allow 80/tcp
sudo ufw allow 8443/tcp
sudo ufw allow 19132/udp
sudo ufw allow 19133/udp
# Antes de habilitar UFW, permite SSH solo desde tu IP administrativa:
# sudo ufw allow from TU_IP_PUBLICA to any port 22 proto tcp
# No abras SSH a todo Internet.
~~~

## Desplegar el broker API

El broker de voz está implementado en la rama Draft `feature/voice-admission-broker-v1` de cinnabar-api PR #1. No se considera desplegado por el hecho de que el código esté en esa rama. Primero deben pasar sus pruebas y aprobarse el PR; luego se despliega el Worker por el flujo de producción existente.

Después de desplegar esa versión, configura sus secretos en el entorno protegido de Cloudflare:

- `VOICE_ADMISSION_PRIVATE_KEY`: PEM PKCS#8 privado Ed25519.
- `VOICE_SFU_ADMIN_URL`: origen HTTPS del SFU, por ejemplo `https://IP_PUBLICA.nip.io:8443`.
- `VOICE_SFU_ADMIN_TOKEN`: los 32 caracteres imprimibles generados anteriormente.
- Opcionales `VOICE_ADMISSION_ISSUER` y `VOICE_ADMISSION_AUDIENCE`, solo si se desea sobrescribir los valores predeterminados coordinadamente.

No pegues ningún secreto aquí ni lo guardes en el repositorio. Si falta el secreto de firma o el acceso administrativo HTTPS al SFU, el broker debe permanecer cerrado y no emitir admisiones.

## Configuración de ambos clientes

Para que Go reciba estas variables tanto en Windows como en Android, el lanzador las incorpora al proceso hijo al compilar la Alpha. En el repositorio GitHub abre **Settings → Secrets and variables → Actions → Variables** y crea estas dos variables públicas:

- `CINNABAR_VOICE_WSS_ENDPOINT` = `wss://IP_PUBLICA.nip.io:8443/v1/voice`
- `CINNABAR_VOICE_ICE_SERVERS` = `[{"urls":["stun:stun.l.google.com:19302"]}]`

Después ejecuta el workflow **Voice Chat Alpha Builds** para producir el instalador Windows y la APK Android con la misma configuración. Las variables pueden contener únicamente la URL WSS y configuración STUN pública. Para la primera prueba usa STUN sin usuario ni contraseña. El código del lanzador rechaza los valores ICE con URL TURN, nombre de usuario o credencial antes de incorporarlos a la compilación.

**Nunca** guardes en esas variables una clave privada, el token administrativo SFU ni credenciales TURN; esos datos no deben compilarse dentro de los clientes. La entrega de credenciales TURN temporales para redes restrictivas queda pendiente de integrar con el broker. Si las variables no están definidas, la voz queda deshabilitada de forma segura. No introduzcas room ID, session ID o XUID manualmente: el broker obtiene esos datos de la sesión autenticada y del roster FriendHost.

## Comprobación previa

~~~sh
curl -i https://IP_PUBLICA.nip.io:8443/healthz
sudo ss -lntup | grep -E '(:8443|:19132|:19133)'
~~~

El health check debe devolver HTTP 204. Comprueba que el proceso escucha TCP 8443 y los sockets UDP fijados. Después sigue la guía [voice-chat-testing.md](voice-chat-testing.md) para conectar dos dispositivos y probar audio en ambas direcciones, salida de un miembro y cierre de sala.

## Límites actuales

- El broker es FriendHost-scoped; todavía no acredita miembros de servidores Bedrock de terceros.
- No se han probado dos jugadores reales hasta completar y registrar la prueba en dos dispositivos.
- STUN-only no garantiza conexión entre todas las redes; TURN puede ser necesario.
- Las claves, secretos, DNS, certificado, SFU y Worker son configuración de operador; no se incluyen en esta rama.
- Mantén los PR de voz en Draft hasta superar la prueba real y revisar la seguridad de producción.