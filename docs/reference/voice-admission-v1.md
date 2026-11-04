# Voice room admission contract — v1

Status: implementation contract; the remote issuer is not yet deployed.

This is the trust boundary between Cinnabar account/session services and a voice media router. The media router must fail closed when the issuer key, token, session binding, or room registry is unavailable.

## Required trust chain

1. The Cinnabar account service authenticates the player using the existing account session. The player XUID must come from the verified account record, not a request field.
2. A trusted Minecraft integration proves the player's membership in one active partida and supplies its authoritative session identifier. A client address, server display name, local process ID, actor_session_id, or session_generation is not sufficient.
3. The broker creates an opaque room UUID for that exact partida and signs a short-lived admission token with an Ed25519 private key held only by the broker.
4. The SFU verifies the token with a pinned Ed25519 public key, then binds the room UUID to the signed session UUID. It does not accept a client-selected room or participant identity.
5. The broker or session integration revokes admission on account sign-out, server leave, transfer, membership removal, or partida closure. The SFU also expires tokens and removes a peer when its transport closes.

## Admission token

The token is a compact JWT with alg=EdDSA and typ=JWT. The payload fields are:

| Claim | Meaning |
|---|---|
| iss | Exact configured Cinnabar broker issuer |
| aud | Exact configured SFU audience (cinnabar-voice-sfu) |
| sub | Stable Cinnabar account subject |
| xuid | Xbox XUID resolved by the authenticated account service |
| room_id | Opaque canonical UUID minted for this partida |
| session_id | Canonical UUID for the authoritative active Minecraft partida |
| jti | One-use canonical UUID for this admission |
| iat, nbf, exp | Issue, not-before and expiry timestamps in Unix seconds |

Tokens are limited to 4 KiB and five minutes; the broker should issue them for about two minutes. The SFU consumes a token once, rejects a duplicate XUID in a room, checks the room/session binding, and enforces a configured participant cap. Tokens, signatures, ICE credentials and raw audio must never be written to logs.

## Revocation

An active peer must renew its lease before exp (recommended interval: 60 seconds). Renewal uses a newly signed jti for the same account, XUID, room and partida; it replaces the lease without replacing the media connection. The SFU closes peers whose lease expires. It also removes a participant on WebRTC peer close, explicit broker revocation, account sign-out, membership removal, or session/partida close. Closing a session revokes every participant bound to its exact room_id and session_id pair. Reconnect requires a newly issued token; a previous token cannot be replayed. The broker can revoke one active participant immediately with `POST /v1/admin/rooms/{roomID}/admissions/{tokenID}/revoke`; the SFU closes the peer only if that jti is still its current lease, so a stale pre-renewal token cannot terminate the renewed connection. It returns the same `204 No Content` for present and absent admissions to avoid exposing room membership.

## Audio plane

The first Go relay implementation uses WebRTC DTLS-protected SCTP DataChannels, separate from Bedrock packet framing and the Rust/Go control channel. The client creates an unordered, zero-retransmit uplink named cinnabar-voice-uplink-v1; the SFU creates a matching downlink. A frame is exactly 648 bytes: an unsigned 64-bit big-endian sequence followed by 320 little-endian signed PCM16 mono samples at 16 kHz (20 ms). The SFU rejects other sizes, string messages, duplicate/out-of-order sequences and frames that would exceed the small per-peer buffered-byte budget. This is an initial uncompressed transport for two-client validation, not the final bandwidth-efficient Opus/RTP profile. Configure operator-managed ICE/TURN, participant limits and no disk recording. The SFU forwards only within one verified room. PTT release, mute, permission loss and disconnect must close the sender gate before the next frame is accepted.

## Known integration blockers

- The repository contains a native Cinnabar API client, not the server implementation that would mint these tokens.
- Native proxy joins can observe the authenticated upstream Minecraft XUID. That is useful identity evidence but does not prove arbitrary-server partida membership.
- The FriendHost path has Xbox MPSD membership and per-member login nonces, but this proof is local to a FriendHost process. A production broker needs a trusted server-side adapter that can verify that membership; it cannot trust a nonce or room string submitted by an arbitrary client.
- The existing core/webrelay control/play channels carry game control and Bedrock data. They are not the voice media plane.
- The repository now includes a standalone Pion SFU process at core/cmd/voice-sfu, with TLS, pinned broker public-key, and ICE/TURN configuration. No WSS endpoint is deployed and no broker public key is configured in the product core yet.

Until the account service and a trusted partida adapter mint this signed contract, voice-room admission remains disabled. Do not create a fallback based on IP address, server name, client-provided XUID, or an unauthenticated room ID.
