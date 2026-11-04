# Cinnabar Voice Chat — P2P Alpha plan

Status: implementation plan; **not yet an operational transport**.

## Goal

Provide a no-VPS, two-player voice path for Windows and Android using WebRTC peer-to-peer media. Keep the existing SFU transport available as a separate mode and do not remove or weaken its admission checks.

## Trust and room authority

- Reuse the authenticated Cinnabar API WebSocket and the existing FriendHost-scoped room broker.
- The broker remains the only authority for room IDs, game-session IDs, participant XUIDs, admission leases and membership changes.
- Never accept a client-selected room ID, XUID, session ID or peer identity as proof of membership.
- The current FriendHost roster trust limitation remains: the API checks live authenticated accounts but does not independently attest arbitrary third-party Bedrock servers.
- Signaling messages must be authorized against the current stored membership on every message and must be bounded, rate-limited, and rejected after lease expiry or disconnect.

## Proposed two-peer flow

1. FriendHost opens a broker-generated room and reports its authenticated roster using the existing protocol.
2. Each participant requests a short-lived voice admission over its authenticated Cinnabar API connection.
3. The broker designates one participant as offerer deterministically from server-side room state; the other is answerer. Clients cannot nominate an arbitrary peer.
4. The broker relays bounded SDP offer/answer and ICE candidate messages only between the two currently admitted members of that exact room/session.
5. WebRTC carries the audio DataChannel directly between peers. Use configured public STUN servers for candidate discovery.
6. If direct connectivity fails, the client reports a clear connection failure. TURN fallback is a separate deployment requirement; public STUN alone cannot guarantee traversal of restrictive/symmetric NATs.
7. Membership removal, account disconnect, host lease expiry, room close, token renewal failure, or peer disconnect closes the signaling session and media peer.

## Transport compatibility

- Introduce an explicit transport selection (p2p or sfu) with fail-closed validation.
- Keep the current SFU implementation and its PTT acknowledgement, frame bounds, sequence validation, backpressure and revocation behavior unchanged.
- The P2P path must preserve the same PCM frame format, source isolation, receive mixer, mute/PTT policy and bounded queues.
- Do not silently fall back from a failed P2P connection to an untrusted endpoint or an unauthenticated room.

## Required implementation slices

### API broker
- Add authenticated voice_signal message schemas and strict validation.
- Relay only offer/answer/candidate messages between exactly two active members.
- Enforce room/session membership, per-socket message limits, candidate/SDP byte limits, rate limits and lease checks.
- Clear pending signaling state on room close, membership removal, account offline and host lease expiry.
- Add Vitest coverage for unauthorized senders, cross-room isolation, oversized/malformed SDP, candidate floods, stale sessions, disconnect cleanup and exactly-two-peer behavior.

### Native Go transport
- Add a P2P peer role and signaling adapter without coupling the transport to client-supplied identity.
- Implement offerer/answerer negotiation and bounded ICE candidate exchange.
- Reuse existing PCM framing, transmit gate and receive queue contracts.
- Add integration tests with two in-process Pion peers and a broker relay fake; prove bidirectional audio, mute, teardown, renewal and cross-room isolation.

### Packaging and qualification
- Extend the Alpha workflow to build the P2P-capable Windows x64 setup and Android arm64 APK.
- Add automated smoke tests and artifact checks.
- Complete physical Windows-to-Android and Android-to-Windows microphone/speaker tests before calling the Alpha functional.

## Acceptance criteria
- Exactly two authorized clients can establish a direct WebRTC connection and exchange audio in both directions.
- A third or removed account cannot signal or receive audio.
- Closing the host room or either authenticated account tears down the session.
- PTT release stops transmission immediately; opening transmission requires a server acknowledgement.
- Windows x64 EXE and Android arm64 APK are produced by CI and verified.
- Direct connectivity failure is reported honestly; TURN coverage is not claimed until a TURN relay is deployed and tested.

## Current known limitations

This branch starts from the existing SFU-capable foundation. The P2P relay and native peer-role implementation are not yet present. No Cloudflare deployment, production secrets, TURN server, Windows installer, APK or physical-device result is created by this planning commit.