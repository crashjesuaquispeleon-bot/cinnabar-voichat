# Cinnabar Voice SFU

This command runs the Pion WebRTC media router implemented in `core/voice`. It exposes one TLS WebSocket signaling endpoint at `/v1/voice` and a non-sensitive `GET /healthz` health check.

The SFU is **not** the room-token issuer. It has only the broker's Ed25519 public key and fails closed when a token is missing, expired, replayed, bound to another session, or signed by an untrusted key. The private signing key must remain in the Cinnabar broker service.

## Required operator configuration

The following environment variables are required:

- `CINNABAR_VOICE_TLS_CERT_FILE`: TLS certificate file for the public WSS endpoint.
- `CINNABAR_VOICE_TLS_KEY_FILE`: matching TLS private-key file.
- `CINNABAR_VOICE_ISSUER`: exact HTTPS issuer claim used by the broker.
- `CINNABAR_VOICE_BROKER_PUBLIC_KEY`: base64-encoded 32-byte Ed25519 public key; never provide the private key here.
- `CINNABAR_VOICE_ICE_SERVERS`: JSON array of Pion ICE server objects with operator-managed STUN/TURN URLs and, for TURN, short-lived credentials.

Optional variables:

- `CINNABAR_VOICE_LISTEN_ADDR`: listen address; defaults to `127.0.0.1:8443`.
- `CINNABAR_VOICE_AUDIENCE`: audience claim; defaults to `cinnabar-voice-sfu`.
- `CINNABAR_VOICE_MAX_PARTICIPANTS`: maximum participants per room, from 1 to 64; defaults to 8.

Example (replace every placeholder and keep TURN credentials in the deployment secret store):

```sh
export CINNABAR_VOICE_LISTEN_ADDR='127.0.0.1:8443'
export CINNABAR_VOICE_TLS_CERT_FILE='/run/secrets/voice.crt'
export CINNABAR_VOICE_TLS_KEY_FILE='/run/secrets/voice.key'
export CINNABAR_VOICE_ISSUER='https://api.cinnabar.gg/voice'
export CINNABAR_VOICE_AUDIENCE='cinnabar-voice-sfu'
export CINNABAR_VOICE_BROKER_PUBLIC_KEY='<base64 public key only>'
export CINNABAR_VOICE_ICE_SERVERS='[{"urls":["turns:turn.example.net:5349"],"username":"<short-lived user>","credential":"<short-lived credential>"}]'
export CINNABAR_VOICE_MAX_PARTICIPANTS='8'
go run ./cmd/voice-sfu
```

Point the native core's operator-managed `VoiceBridgeConfig.Endpoint` at `wss://<host>/v1/voice` and supply the same ICE server configuration. Do not put the endpoint, public key, TURN credentials, or room ID in user-controlled settings.

## Production boundary still required

This executable does not create admissions. The trusted broker must authenticate the Cinnabar account, obtain authoritative membership in one active Minecraft partida, mint a room UUID, and sign the short-lived token. It must renew and revoke those tokens on account/session lifecycle events. Until that server-side issuer and partida adapter are deployed and configured in the core, the core intentionally leaves `VoiceBridgeConfig` unset and multiplayer voice admission remains disabled.

Use a deployment firewall and rate-limited TLS ingress. Configure TURN for clients behind restrictive NATs; STUN-only deployments will not connect every pair of networks. The SFU does not record or persist audio. Never log admission tokens, TURN credentials, or media frames.
