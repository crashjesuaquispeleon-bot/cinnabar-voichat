# Cinnabar Voice SFU

This command runs the Pion WebRTC media router implemented in `core/voice`. It exposes one TLS WebSocket signaling endpoint at `/v1/voice`, a broker-only room-session close operation, a broker-only individual-admission revoke operation, and a non-sensitive `GET /healthz` health check.

The SFU is **not** the room-token issuer. It has only the broker's Ed25519 public key and fails closed when a token is missing, expired, replayed, bound to another session, or signed by an untrusted key. The private signing key must remain in the Cinnabar broker service.

## Required operator configuration

The following environment variables are required:

- `CINNABAR_VOICE_TLS_CERT_FILE`: TLS certificate file for the public WSS endpoint.
- `CINNABAR_VOICE_TLS_KEY_FILE`: matching TLS private-key file.
- `CINNABAR_VOICE_ISSUER`: exact HTTPS issuer claim used by the broker.
- `CINNABAR_VOICE_BROKER_PUBLIC_KEY`: base64-encoded 32-byte Ed25519 public key; never provide the private key here.
- `CINNABAR_VOICE_CONTROL_TOKEN` **or** `CINNABAR_VOICE_CONTROL_TOKEN_FILE`: base64-encoded random 32-byte bearer secret used only by the trusted broker for room-session teardown and individual-admission revocation. Prefer the file option with systemd `LoadCredential`; do not expose it through `Environment=` or distribute it to clients.
- `CINNABAR_VOICE_ICE_SERVERS`: JSON array of Pion ICE server objects with operator-managed STUN/TURN URLs and, for TURN, short-lived credentials.

Optional variables:

- `CINNABAR_VOICE_LISTEN_ADDR`: listen address; defaults to `127.0.0.1:8443`.
- `CINNABAR_VOICE_AUDIENCE`: audience claim; defaults to `cinnabar-voice-sfu`.
- `CINNABAR_VOICE_MAX_PARTICIPANTS`: maximum participants per room, from 1 to 64; defaults to 8.
- `CINNABAR_VOICE_UDP4_PORT`: fixed IPv4 ICE UDP port; defaults to `19132`.
- `CINNABAR_VOICE_UDP6_PORT`: fixed IPv6 ICE UDP port; defaults to `19133`.
- `CINNABAR_VOICE_UDP4_BIND_IP` and `CINNABAR_VOICE_UDP6_BIND_IP`: optional literal local interface IPs that restrict which addresses the SFU binds. Leave unset to bind every eligible interface.

Example (replace every placeholder and keep TURN credentials in the deployment secret store):

```sh
export CINNABAR_VOICE_LISTEN_ADDR='127.0.0.1:8443'
export CINNABAR_VOICE_TLS_CERT_FILE='/run/secrets/voice.crt'
export CINNABAR_VOICE_TLS_KEY_FILE='/run/secrets/voice.key'
export CINNABAR_VOICE_ISSUER='https://api.cinnabar.gg/voice'
export CINNABAR_VOICE_AUDIENCE='cinnabar-voice-sfu'
export CINNABAR_VOICE_BROKER_PUBLIC_KEY='<base64 public key only>'
export CINNABAR_VOICE_CONTROL_TOKEN_FILE='/run/secrets/voice-control-token' # file contains base64; prefer systemd LoadCredential
export CINNABAR_VOICE_ICE_SERVERS='[{"urls":["turns:turn.example.net:5349"],"username":"<short-lived user>","credential":"<short-lived credential>"}]'
export CINNABAR_VOICE_MAX_PARTICIPANTS='8'
go run ./cmd/voice-sfu
```

Point the native core's operator-managed `VoiceBridgeConfig.Endpoint` at `wss://<host>/v1/voice` and supply the same ICE server configuration. In the current core startup, setting `CINNABAR_VOICE_WSS_ENDPOINT` and a valid `CINNABAR_VOICE_ICE_SERVERS` JSON array enables construction of the bridge configuration; if ICE configuration is missing or malformed, startup logs a warning and leaves the bridge disabled. The signaling endpoint uses TCP/TLS; ICE voice traffic uses fixed UDP `19132` (IPv4) and `19133` (IPv6). Do not put the endpoint, public key, TURN credentials, or room ID in user-controlled settings.

## Important: Bedrock port ownership

Bedrock Dedicated Server already defaults to UDP `19132` for IPv4 and UDP `19133` for IPv6. The SFU cannot bind an address/port while the Minecraft server owns the same UDP socket. Deploy the SFU on a separate host or on a distinct public IP that the Minecraft process does not already bind, or move the Minecraft server to different ports and update the address/port players use to join. On a shared host with multiple IP addresses, set the two `*_BIND_IP` variables to dedicated voice interface addresses only when Bedrock is bound to a different specific address; an existing wildcard Bedrock socket (`0.0.0.0` or `::`) still conflicts with every local address. If you move Bedrock to custom ports, review `enable-lan-visibility`: when enabled, Bedrock can still bind the default discovery ports and recreate the conflict.

Do not solve this by enabling UDP port reuse: that would not reliably route Bedrock RakNet and WebRTC ICE datagrams to the correct process. A same-IP shared socket requires a deliberate protocol-aware multiplexer integrated with the Minecraft server and is not provided by this sidecar.

## Production trust scope and remaining deployment work

The separate [Cinnabar API broker PR](https://github.com/crashjesuaquispeleon-bot/cinnabar-api/pull/1) implements FriendHost-scoped room creation, durable roster membership, short-lived Ed25519 admissions, host lease refresh, guest account-presence checks, and SFU room/member revocation. The native FriendHost reports guest XUIDs only after its Xbox login and MPSD session nonce checks; the broker additionally requires each reported account to have a live authenticated Cinnabar API connection.

This is **not** independent proof of membership for arbitrary third-party Bedrock servers, nor protection against a compromised or modified FriendHost process that lies about its roster. General server support requires an independently trusted MPSD verifier or a server-signed membership assertion. Do not treat client-supplied XUIDs, room IDs, server names, or local actor/session identifiers as membership proof.

The broker integration and core bridge wiring are present in the development branches, but neither service is deployed by this repository change. Production still requires an operator-managed WSS hostname, TLS certificate, Ed25519 public/private key pair (private key only in the broker), matching broker/SFU control token, API secrets, and reachable STUN/TURN infrastructure. The broker must be deployed with the same issuer/audience/key configuration used by the SFU. Configure TURN for clients behind restrictive NATs; STUN-only deployments will not connect every pair of networks.

When a partida closes, the broker calls `POST /v1/admin/rooms/{roomID}/sessions/{sessionID}/close` so every peer in that exact room/session is removed immediately. When a player leaves the FriendHost roster or signs out, the broker calls `POST /v1/admin/rooms/{roomID}/admissions/{tokenID}/revoke` with that admission's current jti. The host refreshes the full roster on a 25-second cadence; the bridge requests a fresh admission token every 60 seconds and fails closed if renewal is rejected. The broker host lease and token expiry are additional safeguards, not substitutes for successful deployment tests.

Both SFU admin operations require the separate control token, reject request bodies and return no participant data. The SFU does not record or persist audio. Never log admission tokens, the control token, TURN credentials, or media frames.


## Operator runbooks

- [Public SFU deployment for the two-player Alpha test](../../../docs/reference/voice-chat-sfu-deployment.md)
- [Two-player real-client integration test](../../../docs/reference/voice-chat-testing.md)
