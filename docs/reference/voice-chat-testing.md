# Voice Chat Alpha — prueba de integración con dos jugadores

> **Estado:** las pruebas automatizadas del SFU ya cubren participantes Pion, audio bidireccional, aislamiento de salas, renovación y revocación. La prueba con dos instalaciones reales sigue pendiente hasta desplegar el Worker broker y el SFU en una máquina pública.

Esta guía corresponde a la implementación actual de la rama `feature/voice-chat-foundation`. El alcance admitido es **Cinnabar FriendHost**. No es una autorización genérica para servidores Bedrock de terceros.

## Requisitos antes de iniciar

1. Dos dispositivos con la misma compilación Alpha de Cinnabar (mismo commit), dos cuentas Microsoft/Xbox distintas y permiso de micrófono concedido.
2. El Worker de Cinnabar API con el broker de voz de [cinnabar-api PR #1](https://github.com/crashjesuaquispeleon-bot/cinnabar-api/pull/1) desplegado y configurado. Los dos usuarios deben mantener una conexión autenticada y activa con la API.
3. Un SFU accesible desde Internet en un host separado del servidor Bedrock, con certificado TLS válido y endpoint WSS.
4. Entrada permitida hacia el SFU: TCP 8443 (o el puerto WSS elegido), UDP 19132 IPv4 y UDP 19133 IPv6. Si el servidor Bedrock vive en otra máquina, sus puertos no se modifican.
5. El proceso nativo de cada cliente debe recibir la configuración de operador:
   - `CINNABAR_VOICE_WSS_ENDPOINT=wss://<host>:8443/v1/voice`
   - `CINNABAR_VOICE_ICE_SERVERS=<JSON STUN/TURN>`
   El SFU usa la misma configuración ICE. No pegues tokens, claves privadas ni credenciales TURN permanentes en tickets o registros públicos.
6. El broker y el SFU deben usar el mismo issuer, audience, clave Ed25519 pública y token de control. La clave privada de firma se queda únicamente en los secretos del Worker.

Consulta [Despliegue del SFU para Alpha](voice-chat-sfu-deployment.md) para preparar un host público.

## Prueba de dos jugadores

### A. Verificar servicios

1. Abre `https://<host>:8443/healthz`. El SFU responde HTTP 204 sin devolver datos de jugadores.
2. Comprueba que el Worker responde en su endpoint de salud y que los secretos del broker están configurados.
3. Comprueba que los registros del servicio no muestran errores de certificado, ICE ni enlace UDP. Nunca copies tokens de admisión ni credenciales.

### B. Conectar a la misma partida

1. Instala la misma compilación Alpha en los dos dispositivos.
2. Inicia sesión en Cinnabar y en Microsoft/Xbox con cuentas diferentes. Mantén abiertas ambas conexiones de Cinnabar API.
3. El jugador anfitrión crea una partida FriendHost y el segundo jugador entra a esa misma partida.
4. Comprueba que el anfitrión y el invitado aparecen en la lista de miembros validada por FriendHost. El cliente no debe enviar por su cuenta XUID, room ID ni session ID para obtener admisión.
5. El broker crea la sala, guarda la lista completa de participantes y entrega una admisión corta a cada miembro autorizado.
6. En ambos clientes abre Settings → Sound, activa Voice Chat, selecciona el micrófono y usa Push to Talk. La tecla inicial es V salvo que se haya cambiado en Controls.

### C. Audio en ambas direcciones

1. El jugador A mantiene PTT y habla durante 5–10 segundos. B debe oírlo sin eco local ni cortes persistentes.
2. B habla mientras A escucha. A debe oír a B.
3. Repite con PTT liberado, Voice Mute activado y desactivado, y cambio de micrófono.
4. Comprueba que el audio no se reproduce al propio emisor y que dos salas distintas no reciben el audio una de otra.

### D. Salida y revocación

1. Con ambos conectados, cierra el juego o desconecta al jugador B.
2. A debe seguir conectado y conservar el audio de la sala; no debe cerrarse su sesión por la salida de B.
3. Vuelve a conectar B y confirma que recibe una admisión nueva, no el token anterior.
4. Cierra la partida FriendHost. Ambos clientes deben dejar de transmitir y reproducir audio de esa sala.
5. Mantén la partida abierta y simula una reconexión del socket de Cinnabar API del anfitrión. FriendHost debe recrear la sala del broker, volver a publicar el roster completo y permitir que ambos clientes obtengan admisiones nuevas sin reiniciar el mundo.
5. Repite una vez con A como emisor y B como receptor.

## Evidencia que se debe registrar

- SHA exacto de la compilación de Cinnabar en ambos dispositivos.
- Versión de Windows/Android, tipo de red (Wi-Fi/datos móviles) y micrófono utilizado.
- Resultado de A→B, B→A, PTT, mute, desconexión de B, reconexión y cierre de sala.
- Estado del endpoint `/healthz` y errores de red relevantes, sin tokens, claves, XUID completos ni credenciales.

Marca la prueba como **aprobada** solo si los dos clientes reales completan las secciones B, C y D. Una prueba Pion automatizada o una compilación exitosa no sustituye esta evidencia.

## Diagnóstico rápido

- WSS falla antes de crear la sesión: revisar DNS, certificado, endpoint y TCP del puerto de señalización.
- WSS conecta pero ICE no establece conexión: revisar UDP 19132/19133, direcciones públicas, reglas del firewall y configuración STUN/TURN. STUN por sí solo no atraviesa todas las redes; algunas conexiones necesitan TURN.
- El anfitrión no abre sala o el invitado no recibe admisión: revisar que ambos sockets de API estén autenticados y que el Worker tenga la clave de firma y el acceso HTTPS al SFU.
- El invitado se conecta pero no oye: comprobar PTT, permiso del micrófono, canal de datos, ICE y que ambos pertenecen a la misma sala/sesión.

No habilites fallback que acepte XUID, room ID, session ID o dirección de servidor suministrados por el cliente.