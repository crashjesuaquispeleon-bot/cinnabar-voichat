# Cinnabar Voice Chat

## Product contract

The first release is a Cinnabar-client-only voice feature.

- **Room mode:** global voice. Every Cinnabar client admitted to the same voice room can hear every other unmuted participant; there is no distance attenuation or proximity filter.
- **Room scope (confirmed by the user):** players can communicate only when they are in the same active Minecraft multiplayer session/partida. The voice room is bound to that exact joined session; players in different servers or different partidas cannot hear each other. "Global" means everyone in that one session can hear everyone else, regardless of in-game distance.
- **Client compatibility:** Cinnabar only. Vanilla Bedrock and Java clients are out of scope.
- **Microphone modes:** open microphone and Push to Talk (PTT), selectable in settings. The default mode and PTT key are not yet selected.
- **User controls:** local mute, microphone permission/status, input/output device selection, input sensitivity and per-user receive volume are candidate controls; their UI placement is not yet designed.

## Repository facts confirmed during initial inspection

- `crates/client-audio` owns client sound playback, audio settings and the Bevy adapter. Its current playback path uses `rodio` and decoded PCM; this is output functionality, not microphone capture.
- `crates/bridge` owns bounded framing for the local Rust/Go process stream. That stream carries Bedrock session frames and control handoff. It should not become the real-time voice media path: voice has different latency, loss, congestion and lifecycle requirements.
- `core/` is the Go integration process. The module already declares Pion WebRTC and the repository already has a browser relay contract in `crates/bridge` / `web/transport`. Their presence does **not** establish that a native voice room, SFU, signaling API or authorization path exists.
- No existing microphone-capture implementation or end-to-end voice-chat feature was identified in the inspected client audio modules.

## Proposed high-level media flow (not implemented)

```text
Cinnabar microphone
  -> capture + permission gate
  -> bounded audio-frame queue
  -> voice codec (selection pending)
  -> authenticated real-time voice transport
  -> room media router / SFU (service ownership pending)
  -> subscribed Cinnabar clients
  -> decode + bounded jitter buffer
  -> client-audio playback mixer
```

A signaling/control plane must authenticate the Cinnabar account, bind the participant to the currently joined Minecraft session, admit it to that session's room, and revoke membership on disconnect or session change. Media routing must not trust client-supplied player identity or room membership.

A server-routed SFU is the leading option to evaluate for global rooms. A full-mesh peer topology scales poorly as participant count grows. Do not select or implement an SFU, codec, or transport until the existing Cinnabar service/API ownership and deployment model have been inspected and an operator-controlled endpoint is identified. The Pion dependency in `core/go.mod` is only a dependency fact, not proof that Pion is the correct or currently available service boundary.

## Architectural constraints

1. Keep capture, encoding, network I/O and decoding off the Bevy frame-critical path. Use bounded queues and explicit backpressure/drop policy; never let a slow receiver stall game packet processing or rendering.
2. Keep real-time voice media separate from Bedrock packet framing. Reuse existing identity and session lifecycle facts where possible, but introduce a dedicated media transport contract.
3. Keep device APIs and capture ownership in a low-level audio/platform owner. Keep Bevy/UI integration in the appropriate client adapter; do not put Bevy dependencies into a domain crate.
4. Do not start capture until the user has granted OS permission and explicitly enabled voice. Open-mic mode may transmit only while enabled; PTT transmits only while the configured key is held. Local mute must stop transmission immediately.
5. On disconnect, reconnect, account change or world-session change, stop capture/transmission and retire room membership, queued frames and remote playback belonging to the old session.
6. Bound frame size, queue depth, participant count, decoder work and jitter-buffer memory. Validate all signaling and media metadata; do not accept arbitrary room IDs or participant identities from an untrusted client.
7. Do not record, persist or upload raw microphone audio. Diagnostics must not include voice payloads, credentials or private room tokens.
8. Preserve Cinnabar's license and third-party notices. Any new codec or capture library requires license, platform and packaging review before adoption.

## Work plan and acceptance gates

### Stage 0 — Architecture and service discovery (current)

- [x] Confirm product choices: global voice within the same active Minecraft session, Cinnabar clients only, open mic + PTT.
- [x] Inspect current client playback, bridge framing and browser relay ownership at a high level.
- [ ] Identify the authoritative Cinnabar account/session identity and the service that can authorize a room for the same joined Minecraft server.
- [ ] Determine whether a maintained voice SFU/service already exists in the wider Cinnabar deployment; if not, document the required service and operational owner.
- [ ] Compare capture/codec/transport options against supported desktop platforms, latency, licensing and packaging.
- [ ] Define a versioned, authenticated signaling contract and bounded media-frame contract before implementing network transport.

### Stage 1 — Local audio vertical slice

- [ ] Implement microphone device enumeration, permission/error states and capture lifecycle on the selected desktop platforms.
- [ ] Implement open-mic and PTT state transitions with tests; no network transmission yet.
- [ ] Implement a bounded capture-to-playback loopback test and verify that capture shutdown is immediate on mute, disconnect and application exit.
- [ ] Add device selection and input-level configuration without changing existing game sound behavior.

### Stage 2 — Authenticated global rooms

- [ ] Implement server-side room admission bound to Cinnabar identity and the active game-session identity.
- [ ] Implement signaling, media routing, participant lifecycle and reconnect behavior.
- [ ] Add remote decode, jitter buffering, receive-volume controls and participant speaking/mute status.
- [ ] Verify two-client and multi-client sessions, session isolation and cleanup under packet loss and reconnect.

### Stage 3 — Product integration and qualification

- [ ] Add settings and in-game voice controls using the existing UI ownership rules.
- [ ] Qualify Windows, macOS and Linux capture/playback and device switching.
- [ ] Measure CPU, memory, bandwidth, end-to-end latency and queue drops at representative room sizes.
- [ ] Document server deployment, configuration, privacy behavior and troubleshooting.

## Confirmed session-isolation behavior

- A voice room belongs to one exact active multiplayer session, not merely to a server address or a global Cinnabar account pool.
- All admitted Cinnabar players in that same partida can talk regardless of distance.
- A player joining another server/session must leave the previous room before joining the new one.
- Disconnecting, changing account, changing world/server session or losing authorization must stop capture/transmission and discard old-session audio.
- The backend must derive/validate the room membership from trusted session state; clients must not choose arbitrary room IDs.

## Open decisions

- Which Cinnabar-owned service/API is authoritative for account identity, current server-session identity and voice-room authorization?
- Is there an existing deployable Cinnabar SFU, or must one be designed and operated separately?
- Which desktop platforms are in the first release? The initial qualification proposal is Windows, macOS and Linux.
- Which codec and media transport meet the selected service's constraints? Opus/WebRTC is a candidate, not a decision.
- What should the default microphone mode and PTT binding be?
- What is the room participant cap and what should happen when the cap is reached?

Until the service and identity questions are answered, the media transport and room implementation remain intentionally incomplete. This document is an architecture proposal, not evidence that voice chat is already functional.
